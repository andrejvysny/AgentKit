//! Workspace coordination: who may change what, and when. Mirrors
//! `packages/toolpacks-coding/src/coordination.ts`.
//!
//! Two kinds of lease share one workspace:
//!  - a FILE lease (shared workspace lease + exclusive lock on one canonical
//!    file) held by edit_file / write_file while they read-check-write;
//!  - a WORKSPACE lease (exclusive) held by a write-capable shell command, so
//!    a native edit cannot overlap it.
//!
//! Waiting is FIFO with writer preference: once a workspace lease is requested
//! no new file lease starts ahead of it. Read-only tools never take a lease.
//!
//! Limitation: an uncoordinated external editor (an IDE, another tool) takes
//! none of these locks. Its changes are caught by the content checks the
//! tools make against the version the session read, not by locking.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};

use async_trait::async_trait;
use futures::future::BoxFuture;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::errors::CodingError;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeaseHolder {
    pub chat_id: Option<String>,
    pub run_id: String,
    pub tool_call_id: Option<String>,
    pub tool: String,
    pub label: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitKind {
    File,
    Workspace,
}

#[derive(Clone, Debug)]
pub struct CoordinationWait {
    pub kind: WaitKind,
    pub root: String,
    pub path: Option<String>,
    pub waiter: LeaseHolder,
    pub holders: Vec<LeaseHolder>,
}

pub type ReleaseFuture = BoxFuture<'static, ()>;
pub(crate) type ReleaseFn = Box<dyn FnOnce() -> ReleaseFuture + Send>;

/// An acquired lease. `release` is idempotent.
pub struct WorkspaceLease {
    releases: Option<Vec<ReleaseFn>>,
}

impl std::fmt::Debug for WorkspaceLease {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkspaceLease")
            .field("held", &self.releases.is_some())
            .finish()
    }
}

impl WorkspaceLease {
    pub(crate) fn new(releases: Vec<ReleaseFn>) -> Self {
        Self {
            releases: Some(releases),
        }
    }

    /// Idempotent.
    pub async fn release(&mut self) {
        // Take ONE closure at a time (in order): if this future is dropped
        // while one release awaits, the rest are still in `self.releases`
        // and `Drop` defers them. The in-flight closure's own future dies
        // with us, so closures should do their effect before awaiting.
        while let Some(release) = self.next_release() {
            release().await;
        }
        self.releases = None;
    }

    fn next_release(&mut self) -> Option<ReleaseFn> {
        let releases = self.releases.as_mut()?;
        if releases.is_empty() {
            None
        } else {
            Some(releases.remove(0))
        }
    }
}

/// Deferred release for closures whose owning future may never complete.
pub(crate) fn spawn_release(release: ReleaseFn) {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            handle.spawn(async move {
                release().await;
            });
        }
        Err(_) => {
            // No runtime on this thread (test teardown, panic unwind outside
            // a task): reclaim on a private one rather than leak the gate.
            std::thread::spawn(move || {
                if let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    runtime.block_on(release());
                }
            });
        }
    }
}

/// TS releases every lease in a `finally`; a Rust future can instead be
/// DROPPED mid-flight — the core tool harness aborts a tool on timeout or
/// cancellation by completing the `select!` and discarding the future, and
/// `abort()` does the same to whole tasks. No code after the last await
/// runs, so the release closures must be reclaimed here or the workspace
/// stays wedged (`workspace_busy` for its whole process lifetime, and with
/// the lock-dir coordinator the stale locks carry this live pid and are
/// never declared stale). Dropping an un-released lease therefore defers
/// its releases to the runtime instead.
impl Drop for WorkspaceLease {
    fn drop(&mut self) {
        if let Some(releases) = self.releases.take() {
            for release in releases {
                spawn_release(release);
            }
        }
    }
}

/// One acquired release that has not been handed to a `WorkspaceLease` yet
/// (e.g. the shared part held while the file part is still being acquired).
/// Same drop-safety contract as [`WorkspaceLease`].
pub(crate) struct PendingRelease {
    release: Option<ReleaseFn>,
}

impl PendingRelease {
    pub(crate) fn new(release: ReleaseFn) -> Self {
        Self {
            release: Some(release),
        }
    }

    /// Explicitly hand the release to a caller-managed path (the TS
    /// `catch { await release(); }` branch).
    pub(crate) fn take(&mut self) -> Option<ReleaseFn> {
        self.release.take()
    }

    /// Hand the release to a `WorkspaceLease`, which now owns its cleanup.
    pub(crate) fn consume(mut self) -> ReleaseFn {
        self.release.take().expect("pending release consumed once")
    }
}

impl Drop for PendingRelease {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            spawn_release(release);
        }
    }
}

#[derive(Clone)]
pub struct AcquireFileInput {
    pub root: String,
    pub path: String,
    pub holder: LeaseHolder,
    pub cancellation: Option<CancellationToken>,
    /// Give up waiting after this long with a `workspace_busy` CodingError.
    pub max_wait_ms: Option<u64>,
    /// Stable identity of the effect this lease guards (for a command: the
    /// owner-pid:uuid marker inherited by every descendant). Durable
    /// ownership records carry it so cleanup evidence names the exact
    /// execution, not just a pid that can be recycled.
    pub execution_id: Option<String>,
}

#[derive(Clone)]
pub struct AcquireWorkspaceInput {
    pub root: String,
    pub holder: LeaseHolder,
    pub cancellation: Option<CancellationToken>,
    pub max_wait_ms: Option<u64>,
    /// See [`AcquireFileInput::execution_id`].
    pub execution_id: Option<String>,
}

/// Evidence that cleanup was not positively established: the command's
/// process tree still had live members when the lease was released.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuarantineEvidence {
    pub pids: Vec<i32>,
    pub holder: LeaseHolder,
    /// Fixed machine code for why cleanup was not established (for example
    /// `driver_panic`, `group_stuck`). Never command text, so it is stored and
    /// shown in clear.
    pub reason: Option<String>,
}

/// SAFE process identity for cleanup evidence (ADR 0020 / B5). A bare pid is
/// never sufficient evidence of anything: pids are recycled. A command's
/// cleanup record therefore names the stable execution nonce
/// (`{owner_pid}:{uuid}`, inherited by every descendant through the
/// `AGENTKIT_EXEC_ID` marker), the process group the command was spawned
/// into, the spawn wall-clock time, and the owner pid whose death proves the
/// run crashed. Positive exit requires proving the MARKED processes are gone
/// and the GROUP is empty — not merely that a pid answers `kill(pid, 0)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandCleanupIdentity {
    /// Stable execution nonce `{owner_pid}:{uuid}` — the marker inherited by
    /// every descendant of the command.
    pub execution_id: String,
    /// The host process that spawned the command; its death means the run
    /// crashed without running teardown.
    pub owner_pid: i64,
    /// The child's process-group id (`setpgid`); `None` when the spawn was
    /// never confirmed (pre-spawn record, or a spawn that failed before the
    /// pgid was reported) — cleanup then rests on the marker scan alone.
    pub pgid: Option<i32>,
    /// Wall-clock anchor (ms since epoch) for the suspect window: the real
    /// spawn time once the driver confirmed it, otherwise the publication
    /// time of the pre-spawn record. Any process that could carry the
    /// execution nonce is younger than this stamp.
    pub spawned_at_ms: u64,
}

/// Durable pre-spawn / pre-admission intent (B5). Written BEFORE the
/// workspace lease is admitted and BEFORE the command spawns, so a crash
/// anywhere in admit → spawn → effect → completion leaves an identifiable
/// ownership record naming the exact execution, its owner pid, and (once
/// known) its process group. The startup reconciliation consults these
/// records to decide whether a crashed run's process group may still be
/// writing into a workspace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutionIntent {
    pub root: String,
    pub identity: CommandCleanupIdentity,
    pub holder: LeaseHolder,
    /// True once the driver reported the child's pgid (spawn confirmed).
    /// An intent whose owner is dead and whose group is positively empty,
    /// with no marked processes, may be retired as "never spawned" or
    /// "fully exited".
    pub spawned: bool,
}

/// Unresolved-cleanup record for a workspace root (ADR 0020). A quarantine
/// is lifted only by positive evidence — every recorded process exited —
/// or by explicit recovery via [`WorkspaceCoordinator::clear_workspace_quarantine`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceQuarantine {
    pub root: String,
    pub pids: Vec<i32>,
    pub holder: LeaseHolder,
    pub quarantined_at_ms: u64,
    /// See [`QuarantineEvidence::reason`]; absent in older records.
    pub reason: Option<String>,
    /// SAFE identity for a command quarantine (B5). `Some` upgrades the
    /// positive-exit proof from a bare pid probe to the marker scan +
    /// process-group probe; `None` is generic evidence (no execution
    /// nonce known), which keeps the pid-only rule.
    pub identity: Option<CommandCleanupIdentity>,
}

/// Cleanup is positively established when the record lists processes and
/// none of them is alive. An empty pid list is an *unknown* survivor set
/// (e.g. an unreadable durable record) and stays blocked until explicit
/// recovery; a pid of `-1` is never probed.
///
/// This is the pid-only rule for evidence that carries NO execution
/// identity. Command quarantines (identity `Some`) must use
/// [`crate::process::cleanup_positively_established`], which additionally
/// requires the execution marker to be absent and the process group empty —
/// never pid alone (pids are recycled).
pub(crate) fn all_quarantined_exited(pids: &[i32]) -> bool {
    !pids.is_empty()
        && pids
            .iter()
            .all(|pid| !crate::process::owner_alive(*pid as i64))
}

/// Identity-aware positive-exit proof for a quarantine record (B5).
pub(crate) async fn quarantine_positively_exited(quarantine: &WorkspaceQuarantine) -> bool {
    match &quarantine.identity {
        // Generic evidence without an execution nonce: pid-only rule.
        None => all_quarantined_exited(&quarantine.pids),
        Some(identity) => {
            crate::process::cleanup_positively_established(identity, &quarantine.pids).await
        }
    }
}

pub(crate) fn quarantine_error(quarantine: &WorkspaceQuarantine) -> CodingError {
    let who = if quarantine.pids.is_empty() {
        "an unknown surviving process".to_owned()
    } else {
        format!(
            "process(es) {}",
            quarantine
                .pids
                .iter()
                .map(i32::to_string)
                .collect::<Vec<_>>()
                .join(",")
        )
    };
    let nonce = quarantine
        .identity
        .as_ref()
        .map(|identity| format!(" execution {}", identity.execution_id))
        .unwrap_or_default();
    let reason = quarantine
        .reason
        .as_ref()
        .map(|reason| format!(" [reason: {reason}]"))
        .unwrap_or_default();
    CodingError::new(
        "workspace_quarantined",
        format!(
            "workspace cleanup is unresolved: {who} from {} '{}' (run {}{nonce}){reason} may still be writing; the workspace stays blocked until the processes exit or recovery is recorded.",
            quarantine.holder.tool, quarantine.holder.label, quarantine.holder.run_id
        ),
    )
}

/// A live child a quarantine record can name as its survivor set, killed and
/// reaped on EVERY exit path: a leaked `sleep 60` keeps its pid "alive" and
/// would leave every later acquisition in the process — and after it, the test
/// suite — blocked on evidence nobody can clear.
#[cfg(test)]
pub(crate) struct ChildGuard(Option<std::process::Child>);

#[cfg(test)]
impl ChildGuard {
    pub(crate) fn spawn(program: &str, args: &[&str]) -> (i32, Self) {
        let child = std::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn survivor child");
        let pid = child.id() as i32;
        (pid, Self(Some(child)))
    }

    /// Kill, reap, and prove the pid is really gone before the test ends.
    pub(crate) fn reap(mut self) {
        let Some(mut child) = self.0.take() else {
            return;
        };
        let pid = child.id() as i32;
        let _ = child.kill();
        let _ = child.wait();
        assert!(
            !crate::process::owner_alive(i64::from(pid)),
            "survivor {pid} is still alive after kill+wait"
        );
    }
}

#[cfg(test)]
impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[async_trait]
pub trait WorkspaceCoordinator: Send + Sync {
    /// Shared workspace lease + exclusive lock on one canonical file. Waits
    /// while a workspace-exclusive lease is held or requested.
    async fn acquire_file(&self, input: AcquireFileInput) -> Result<WorkspaceLease, CodingError>;
    /// Exclusive workspace lease for a write-capable command. Waits for
    /// in-flight file leases; blocks new ones (writer preference, FIFO).
    async fn acquire_workspace(
        &self,
        input: AcquireWorkspaceInput,
    ) -> Result<WorkspaceLease, CodingError>;
    /// Record that a released workspace still had live processes; every
    /// later acquisition of that root is blocked until cleanup is positively
    /// established or recovery is recorded. A durable publication failure is
    /// returned as `Err`: callers must keep the modifying lease and treat the
    /// workspace as blocked — losing the quarantine record while processes
    /// may still write must never look like success.
    async fn quarantine_workspace(
        &self,
        root: &str,
        evidence: &QuarantineEvidence,
    ) -> Result<(), CodingError>;
    /// Live quarantine for the root, if any. Records whose processes have
    /// all exited are cleared here (positive cleanup evidence).
    async fn workspace_quarantine(
        &self,
        root: &str,
    ) -> Result<Option<WorkspaceQuarantine>, CodingError>;
    /// Explicit operator recovery from an unresolved state. Returns whether
    /// a record was removed.
    async fn clear_workspace_quarantine(&self, root: &str) -> Result<bool, CodingError>;
    /// Publish unresolved-cleanup evidence for a COMMAND, carrying the SAFE
    /// process identity (execution nonce, owner pid, process group, spawn
    /// time) so a crash between the lease release and a later reopen can
    /// still reconcile the exact execution's process group. A durable
    /// publication failure is `Err`: the caller keeps the lease blocked
    /// (live processes without durable evidence must never look like
    /// success).
    async fn quarantine_execution(
        &self,
        root: &str,
        evidence: &QuarantineEvidence,
        identity: &CommandCleanupIdentity,
    ) -> Result<(), CodingError>;
    /// Durably record the pre-spawn / pre-admission intent of one command
    /// execution (B5): who holds the effect (holder), its stable execution
    /// nonce, its owner pid, and once known its process group and spawn
    /// time. Upsert — the driver calls it again with `spawned: true` and the
    /// pgid once the child exists. A publication failure is `Err` and the
    /// caller must NOT spawn the command.
    async fn record_execution_intent(&self, intent: &ExecutionIntent) -> Result<(), CodingError>;
    /// Retire an intent whose execution completed its cleanup chain.
    async fn clear_execution_intent(
        &self,
        root: &str,
        execution_id: &str,
    ) -> Result<(), CodingError>;
    /// Startup reconciliation for one root (B5): every intent whose owner
    /// process is dead is a crashed run's declared effect. It is retired
    /// only on POSITIVE evidence — the execution marker names no live
    /// process and the recorded process group is empty — otherwise the root
    /// is quarantined with the intent's safe identity (an unreadable
    /// intent, a scan the platform cannot perform, or a live group all keep
    /// the workspace blocked until recovery is recorded). A blocking intent
    /// is retired only once its replacement evidence is durably established
    /// (a coordinator that quarantines in memory alone keeps the intent;
    /// explicit `clear_workspace_quarantine` retires both). Called from the
    /// lease-acquisition path before any modifying work is admitted.
    async fn reconcile_execution_intents(&self, root: &str) -> Result<(), CodingError>;
}

pub trait CoordinatorObserver: Send + Sync {
    fn on_waiting(&self, _wait: &CoordinationWait) {}
    fn on_acquired(&self, _wait: &CoordinationWait) {}
}

/// realpath of the nearest existing ancestor + the not-yet-existing rest.
async fn realpath_loose(path: &str) -> String {
    let mut missing: Vec<String> = vec![];
    let mut current = if path.starts_with('/') {
        path.to_owned()
    } else {
        format!("/{path}")
    };
    loop {
        if let Ok(resolved) = tokio::fs::canonicalize(&current).await {
            let mut resolved = resolved.to_string_lossy().into_owned();
            if !missing.is_empty() {
                if resolved == "/" {
                    resolved = String::new();
                }
                return format!("{resolved}/{}", missing.join("/"));
            }
            return resolved;
        }
        let trimmed = current.trim_end_matches('/');
        let trimmed = if trimmed.is_empty() { "/" } else { trimmed };
        if trimmed == "/" {
            if missing.is_empty() {
                return "/".to_owned();
            }
            return format!("/{}", missing.join("/"));
        }
        let at = trimmed.rfind('/').unwrap_or(0);
        missing.insert(0, trimmed[at + 1..].to_owned());
        current = if at == 0 {
            "/".to_owned()
        } else {
            trimmed[..at].to_owned()
        };
    }
}

pub async fn canonical_root_key(root: &str) -> String {
    realpath_loose(root).await
}

/// Symlink aliases collapse: an existing target resolves fully; a missing
/// one is realpath(parent) + basename.
pub async fn canonical_file_key(path: &str) -> String {
    let full = if path.starts_with('/') {
        path.to_owned()
    } else {
        format!("/{path}")
    };
    if let Ok(resolved) = tokio::fs::canonicalize(&full).await {
        return resolved.to_string_lossy().into_owned();
    }
    let trimmed = full.trim_end_matches('/');
    let at = trimmed.rfind('/');
    let parent = match at {
        None | Some(0) => "/".to_owned(),
        Some(at) => trimmed[..at].to_owned(),
    };
    let base = match at {
        Some(at) => trimmed[at + 1..].to_owned(),
        None => trimmed.to_owned(),
    };
    let loose = realpath_loose(&parent).await;
    if loose == "/" {
        format!("/{base}")
    } else {
        format!("{loose}/{base}")
    }
}

pub(crate) fn abort_error() -> CodingError {
    CodingError::new("aborted", "the call was aborted")
}

/// Notifies the observer at most once per request, however many phases wait.
#[derive(Default)]
pub(crate) struct WaitTrackerState {
    fired: bool,
    last: Option<CoordinationWait>,
}

#[derive(Clone, Default)]
pub(crate) struct WaitTracker {
    state: Arc<Mutex<WaitTrackerState>>,
}

impl WaitTracker {
    pub(crate) fn waiting(
        &self,
        wait: CoordinationWait,
        observer: Option<&Arc<dyn CoordinatorObserver>>,
    ) {
        let fire = {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            state.last = Some(wait.clone());
            if state.fired {
                false
            } else {
                state.fired = true;
                true
            }
        };
        if fire && let Some(observer) = observer {
            observer.on_waiting(&wait);
        }
    }

    /// Holders of the most recent blocked phase, for the busy message.
    pub(crate) fn last_holders(&self) -> Vec<LeaseHolder> {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .last
            .as_ref()
            .map(|wait| wait.holders.clone())
            .unwrap_or_default()
    }

    pub(crate) fn acquired(&self, observer: Option<&Arc<dyn CoordinatorObserver>>) {
        // Copy under the lock, call the observer after unlocking.
        let wait = {
            let snapshot = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            if snapshot.fired {
                snapshot.last.clone()
            } else {
                None
            }
        };
        if let (Some(wait), Some(observer)) = (wait, observer) {
            observer.on_acquired(&wait);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Shared,
    Exclusive,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct EntryId(u64);

fn next_entry_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

struct Entry {
    id: EntryId,
    mode: Mode,
    holder: LeaseHolder,
    grant: Option<oneshot::Sender<()>>,
}

/// FIFO reader/writer gate with writer preference; a mutex is
/// exclusive-only. Grants are tracked by [`EntryId`], never by holder
/// equality: two requests may carry equal `LeaseHolder`s (no tool-call id,
/// or a reused one) and must not release each other's grant.
#[derive(Default)]
struct Gate {
    shared: Vec<(EntryId, LeaseHolder)>,
    exclusive: Option<(EntryId, LeaseHolder)>,
    queue: VecDeque<Entry>,
}

impl Gate {
    fn holders(&self) -> Vec<LeaseHolder> {
        match &self.exclusive {
            Some((_, holder)) => vec![holder.clone()],
            None => self.shared.iter().map(|(_, h)| h.clone()).collect(),
        }
    }

    fn idle(&self) -> bool {
        self.exclusive.is_none() && self.shared.is_empty() && self.queue.is_empty()
    }

    fn can_grant(&self, mode: Mode) -> bool {
        match mode {
            Mode::Exclusive => self.exclusive.is_none() && self.shared.is_empty(),
            Mode::Shared => self.exclusive.is_none(),
        }
    }

    fn take(&mut self, id: EntryId, mode: Mode, holder: &LeaseHolder) {
        match mode {
            Mode::Exclusive => self.exclusive = Some((id, holder.clone())),
            Mode::Shared => self.shared.push((id, holder.clone())),
        }
    }

    /// Drops the grant owned by exactly this entry, if any.
    fn remove_grant(&mut self, id: EntryId) {
        if self.exclusive.as_ref().is_some_and(|(held, _)| *held == id) {
            self.exclusive = None;
        }
        if let Some(at) = self.shared.iter().position(|(held, _)| *held == id) {
            self.shared.remove(at);
        }
    }

    fn pump(&mut self) {
        while let Some(front) = self.queue.front() {
            if !self.can_grant(front.mode) {
                return;
            }
            let mut entry = self.queue.pop_front().expect("front checked");
            self.take(entry.id, entry.mode, &entry.holder);
            if let Some(grant) = entry.grant.take() {
                let _ = grant.send(());
            }
        }
    }
}

enum AcquireState {
    Granted(EntryId),
    Queued {
        id: EntryId,
        receiver: oneshot::Receiver<()>,
        /// Holders at enqueue time, reported to `on_block` once the reclaim
        /// guard exists and the gate lock is released.
        blocked_by: Vec<LeaseHolder>,
    },
}

/// Reclaims a queued acquisition whose future never completes. A still
/// queued entry is only removed (it holds nothing, so no holder state is
/// touched); an entry that was granted but not yet accepted has its OWN
/// grant removed, by id. Then the gate is re-pumped and dropped when idle.
/// Disarmed once the waiter is granted and accepted (the release closure
/// owns the grant from then on).
struct AcquireGuard {
    map: Arc<GateMap>,
    key: String,
    id: EntryId,
    armed: bool,
}

impl Drop for AcquireGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut gates = lock(&self.map.gates);
        let Some(gate) = gates.get_mut(&self.key) else {
            return;
        };
        if let Some(at) = gate.queue.iter().position(|entry| entry.id == self.id) {
            gate.queue.remove(at);
        } else {
            gate.remove_grant(self.id);
        }
        gate.pump();
        if gate.idle() {
            gates.remove(&self.key);
        }
    }
}

/// Poison-tolerant lock: a panicking observer or test must not wedge every
/// later acquisition behind a poisoned mutex.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

struct GateMap {
    gates: Mutex<HashMap<String, Gate>>,
}

impl GateMap {
    fn new() -> Self {
        Self {
            gates: Mutex::new(HashMap::new()),
        }
    }

    /// Try the fast path; on block, enqueue a waiter. The observer is NOT
    /// called here (it must run outside the lock): `wait` reports
    /// `blocked_by` after unlocking.
    fn try_acquire(&self, key: &str, mode: Mode, holder: &LeaseHolder) -> AcquireState {
        let mut gates = lock(&self.gates);
        let gate = gates.entry(key.to_owned()).or_default();
        let id = EntryId(next_entry_id());
        if gate.queue.is_empty() && gate.can_grant(mode) {
            gate.take(id, mode, holder);
            return AcquireState::Granted(id);
        }
        let blocked_by = gate.holders();
        let (sender, receiver) = oneshot::channel();
        gate.queue.push_back(Entry {
            id,
            mode,
            holder: holder.clone(),
            grant: Some(sender),
        });
        AcquireState::Queued {
            id,
            receiver,
            blocked_by,
        }
    }

    /// Wait for the queued grant, honoring cancellation. An entry already
    /// granted by `pump` still runs its release afterwards. Every EARLY
    /// exit — including the future being dropped mid-await by the core
    /// harness, or `on_block` panicking — is reclaimed by [`AcquireGuard`].
    /// Returns the id that owns the grant.
    async fn wait(
        self: &Arc<Self>,
        key: &str,
        state: AcquireState,
        cancellation: &Option<CancellationToken>,
        on_block: &(dyn Fn(&[LeaseHolder]) + Sync),
    ) -> Result<EntryId, CodingError> {
        let (id, mut receiver, blocked_by) = match state {
            AcquireState::Granted(id) => return Ok(id),
            AcquireState::Queued {
                id,
                receiver,
                blocked_by,
            } => (id, receiver, blocked_by),
        };
        let mut guard = AcquireGuard {
            map: Arc::clone(self),
            key: key.to_owned(),
            id,
            armed: true,
        };
        on_block(&blocked_by);
        let outcome = if let Some(token) = cancellation {
            tokio::select! {
                biased;
                granted = &mut receiver => granted.map_err(|_| abort_error()),
                _ = token.cancelled() => {
                    // Prefer a grant that landed with the cancellation: a
                    // vanished queue entry means `pump` already installed
                    // this entry's grant, and the lease must still be released.
                    let granted = {
                        let gates = lock(&self.gates);
                        gates.get(key).is_some_and(|gate| {
                            !gate.queue.iter().any(|entry| entry.id == id)
                        })
                    };
                    if granted {
                        Ok(())
                    } else {
                        Err(abort_error())
                    }
                }
            }
        } else {
            receiver.await.map_err(|_| abort_error())
        };
        if outcome.is_ok() {
            // The release closure now owns the grant's cleanup.
            guard.armed = false;
        }
        outcome.map(|()| id)
    }

    fn release(&self, key: &str, id: EntryId) {
        let mut gates = lock(&self.gates);
        let Some(gate) = gates.get_mut(key) else {
            return;
        };
        gate.remove_grant(id);
        gate.pump();
        if gate.idle() {
            gates.remove(key);
        }
    }
}

fn sync_release(map: Arc<GateMap>, key: String, id: EntryId) -> ReleaseFn {
    Box::new(move || {
        Box::pin(async move {
            map.release(&key, id);
        })
    })
}

pub(crate) fn busy_error(holders: &[LeaseHolder], waited_ms: u64) -> CodingError {
    let who = if holders.is_empty() {
        "another session holds the workspace".to_owned()
    } else {
        holders
            .iter()
            .map(|holder| {
                format!(
                    "another session is running {}: {}",
                    holder.tool, holder.label
                )
            })
            .collect::<Vec<_>>()
            .join("; ")
    };
    CodingError::new(
        "workspace_busy",
        format!("{who} (waited {waited_ms}ms). Try again later."),
    )
}

/// Run an acquisition under an optional wait cap. The cap cancels the wait
/// (not the work after it) and surfaces as `workspace_busy` naming the
/// holders.
pub(crate) async fn with_wait_cap<T, F>(
    cancellation: Option<CancellationToken>,
    max_wait_ms: Option<u64>,
    tracker: &WaitTracker,
    run: F,
) -> Result<T, CodingError>
where
    F: FnOnce(Option<CancellationToken>) -> BoxFuture<'static, Result<T, CodingError>>,
{
    let Some(max_wait_ms) = max_wait_ms else {
        return run(cancellation).await;
    };
    // A child token follows the caller's cancellation without a forwarder
    // task, and the timer is a `select!` arm on this future itself, so
    // dropping the future leaves no task (or timer) behind.
    let cap = match &cancellation {
        Some(source) => source.child_token(),
        None => CancellationToken::new(),
    };
    let busy = Arc::new(AtomicBool::new(false));
    let mut running = run(Some(cap.clone()));
    let timer = tokio::time::sleep(std::time::Duration::from_millis(max_wait_ms));
    tokio::pin!(timer);
    let result = tokio::select! {
        biased;
        result = &mut running => result,
        () = &mut timer => {
            busy.store(true, Ordering::SeqCst);
            cap.cancel();
            running.await
        }
    };
    result.map_err(|error| {
        let caller_aborted = cancellation
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled);
        // An unresolved-cleanup refusal keeps its own code: it is not a
        // "someone is busy" wait and recovery does not come from retrying.
        if busy.load(Ordering::SeqCst) && !caller_aborted && error.code != "workspace_quarantined" {
            busy_error(&tracker.last_holders(), max_wait_ms)
        } else {
            error
        }
    })
}

/// In-process coordinator with the tracker exposed so wrappers can share it.
#[derive(Clone)]
pub struct InProcessCoordinator {
    workspaces: Arc<GateMap>,
    files: Arc<GateMap>,
    observer: Option<Arc<dyn CoordinatorObserver>>,
    quarantines: Arc<Mutex<HashMap<String, WorkspaceQuarantine>>>,
    /// Pre-spawn intents by canonical root then execution nonce. Mirrored to
    /// a durable journal under the system temp directory (B5): the point of
    /// the intent is that it outlives THIS process, so a crashed run's
    /// declared effect can be reconciled on the next boot.
    intents: Arc<Mutex<HashMap<String, HashMap<String, ExecutionIntent>>>>,
}

/// Directory of the durable pre-spawn intent journal. The startup
/// reconciliation of every coordinator kind consults its own storage; the
/// in-process coordinator's storage is this directory, keyed by
/// `sha256(canonical root)`.
///
/// The directory is per-user (`agentkit-exec-intents-<uid>`) and private
/// (`0o700`): `$TMPDIR` is commonly world-writable, and these records name
/// pids and process groups that reconciliation later trusts.
pub(crate) fn intent_journal_dir() -> std::path::PathBuf {
    #[cfg(unix)]
    {
        std::env::temp_dir().join(format!("agentkit-exec-intents-{}", current_uid()))
    }
    #[cfg(not(unix))]
    {
        std::env::temp_dir().join("agentkit-exec-intents")
    }
}

#[cfg(unix)]
pub(crate) fn current_uid() -> u32 {
    // SAFETY: `geteuid` takes no arguments, touches no memory and cannot fail.
    unsafe { libc::geteuid() }
}

fn journal_io_error(error: &std::io::Error) -> CodingError {
    CodingError::new("io_error", format!("{error}"))
}

/// Create one directory level with mode `0o700`; an existing one is fine
/// (it is verified separately, never trusted).
fn create_private_dir(path: &std::path::Path) -> std::io::Result<()> {
    #[cfg(unix)]
    let created = {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o700).create(path)
    };
    #[cfg(not(unix))]
    let created = std::fs::create_dir(path);
    match created {
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        other => other,
    }
}

/// Fail closed unless `path` is a real directory (not a symlink) owned by
/// the current user with no group/other access. `Ok(false)` = it does not
/// exist (callers treat that as "no records").
#[cfg(unix)]
fn check_private_dir(path: &std::path::Path) -> Result<bool, CodingError> {
    use std::os::unix::fs::MetadataExt;
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(journal_io_error(&error)),
    };
    let refuse = |why: &str| {
        Err(CodingError::new(
            "io_error",
            format!("execution journal directory {} {why}", path.display()),
        ))
    };
    if !meta.file_type().is_dir() {
        return refuse("is not a directory (symlinks are refused)");
    }
    if meta.uid() != current_uid() {
        return refuse("is not owned by the current user");
    }
    if meta.mode() & 0o077 != 0 {
        return refuse("is accessible by group or other users");
    }
    Ok(true)
}

#[cfg(not(unix))]
fn check_private_dir(path: &std::path::Path) -> Result<bool, CodingError> {
    Ok(path.is_dir())
}

/// The root's journal directory for WRITING: created private on first use
/// and verified on every use. Blocking syscalls run on the blocking pool,
/// never on an async worker.
async fn ensure_journal_dir(root_key: &str) -> Result<std::path::PathBuf, CodingError> {
    let root_key = root_key.to_owned();
    join_blocking(tokio::task::spawn_blocking(move || {
        ensure_journal_dir_sync(&root_key)
    }))
    .await
}

fn ensure_journal_dir_sync(root_key: &str) -> Result<std::path::PathBuf, CodingError> {
    let base = intent_journal_dir();
    create_private_dir(&base).map_err(|error| journal_io_error(&error))?;
    check_private_dir(&base)?;
    let leaf = intent_dir_for(root_key);
    create_private_dir(&leaf).map_err(|error| journal_io_error(&error))?;
    check_private_dir(&leaf)?;
    Ok(leaf)
}

async fn join_blocking<T>(
    handle: tokio::task::JoinHandle<Result<T, CodingError>>,
) -> Result<T, CodingError> {
    handle
        .await
        .map_err(|error| CodingError::new("io_error", format!("journal task failed: {error}")))?
}

/// The root's journal directories for READING / removing: the per-user
/// directory, then (unix) the legacy shared `agentkit-exec-intents/` one that
/// older builds wrote. Missing directories are skipped; a present-but-
/// untrusted per-user one is an error.
async fn existing_journal_dirs(root_key: &str) -> Result<Vec<std::path::PathBuf>, CodingError> {
    let root_key = root_key.to_owned();
    join_blocking(tokio::task::spawn_blocking(move || {
        existing_journal_dirs_sync(&root_key)
    }))
    .await
}

fn existing_journal_dirs_sync(root_key: &str) -> Result<Vec<std::path::PathBuf>, CodingError> {
    let mut dirs = Vec::new();
    if check_private_dir(&intent_journal_dir())? {
        let leaf = intent_dir_for(root_key);
        if check_private_dir(&leaf)? {
            dirs.push(leaf);
        }
    }
    #[cfg(unix)]
    dirs.extend(legacy_journal_leaf(root_key));
    Ok(dirs)
}

/// Pending records written by builds that used one world-shared
/// `agentkit-exec-intents/` directory must still be honored after an
/// upgrade, or an unresolved cleanup would be forgotten (fail open). Only a
/// real directory owned by THIS user is read; anything else belongs to other
/// users (the old directory was shared) or is untrusted, and is ignored
/// with a one-time warning.
#[cfg(unix)]
fn legacy_journal_leaf(root_key: &str) -> Option<std::path::PathBuf> {
    use std::os::unix::fs::MetadataExt;
    static WARNED: std::sync::Once = std::sync::Once::new();
    let ours = |path: &std::path::Path| -> Option<bool> {
        let meta = std::fs::symlink_metadata(path).ok()?;
        Some(meta.file_type().is_dir() && meta.uid() == current_uid())
    };
    let base = std::env::temp_dir().join("agentkit-exec-intents");
    match ours(&base) {
        None => return None,
        Some(false) => {
            WARNED.call_once(|| {
                eprintln!(
                    "Warning: ignoring legacy execution journal {}: not a directory owned by the current user",
                    base.display()
                );
            });
            return None;
        }
        Some(true) => {}
    }
    let leaf = base.join(crate::fs_util::sha256(root_key.as_bytes()));
    (ours(&leaf) == Some(true)).then_some(leaf)
}

/// Create-new write, owner-only (`0o600`) from the first byte.
async fn write_private_file(path: &std::path::Path, text: &str) -> Result<(), CodingError> {
    use tokio::io::AsyncWriteExt;
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(path)
        .await
        .map_err(|error| journal_io_error(&error))?;
    file.write_all(text.as_bytes())
        .await
        .map_err(|error| journal_io_error(&error))?;
    file.flush().await.map_err(|error| journal_io_error(&error))
}

/// A pid / pgid from untrusted JSON: must fit `i32` and be positive.
/// Anything else (0, negative, huge) would let `kill(-1, ..)` style values
/// through, so the whole record is rejected as corrupt instead.
pub(crate) fn parse_pid(value: &serde_json::Value) -> Option<i32> {
    i32::try_from(value.as_i64()?).ok().filter(|pid| *pid > 0)
}

/// Hex sha256 of a holder label: records store the digest, never the text.
pub(crate) fn label_digest(label: &str) -> String {
    crate::fs_util::sha256(label.as_bytes())
}

/// The label a reader reconstructs: the short digest of a current record
/// (the clear text was never persisted), or `unknown`. A legacy record's
/// clear `label` is deliberately ignored.
fn label_from_record(value: &serde_json::Value) -> String {
    value
        .get("labelSha256")
        .and_then(serde_json::Value::as_str)
        .map_or_else(
            || "unknown".to_owned(),
            |digest| format!("sha256:{}", digest.chars().take(12).collect::<String>()),
        )
}

/// Filesystem-safe filename for an execution nonce (`{pid}:{uuid}`).
pub(crate) fn intent_file_name(execution_id: &str) -> String {
    execution_id.replace([':', '/', '\\', ' '], "-")
}

pub(crate) fn now_ms_u64() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_millis() as u64)
        .unwrap_or_default()
}

pub(crate) fn intent_json(intent: &ExecutionIntent) -> serde_json::Value {
    serde_json::json!({
        "root": intent.root,
        "executionId": intent.identity.execution_id,
        "ownerPid": intent.identity.owner_pid,
        "pgid": intent.identity.pgid,
        "spawnedAtMs": intent.identity.spawned_at_ms,
        "spawned": intent.spawned,
        "runId": intent.holder.run_id,
        "chatId": intent.holder.chat_id,
        "toolCallId": intent.holder.tool_call_id,
        "tool": intent.holder.tool,
        "labelSha256": label_digest(&intent.holder.label),
    })
}

pub(crate) fn intent_from_json(value: &serde_json::Value) -> Option<ExecutionIntent> {
    let execution_id = value.get("executionId")?.as_str()?.to_owned();
    let owner_pid = i64::from(parse_pid(value.get("ownerPid")?)?);
    let pgid = match value.get("pgid") {
        None | Some(serde_json::Value::Null) => None,
        Some(pgid) => Some(parse_pid(pgid)?),
    };
    let text = |name: &str| {
        value
            .get(name)
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown")
            .to_owned()
    };
    Some(ExecutionIntent {
        root: value
            .get("root")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        identity: CommandCleanupIdentity {
            execution_id,
            owner_pid,
            pgid,
            spawned_at_ms: value
                .get("spawnedAtMs")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or_default(),
        },
        holder: LeaseHolder {
            chat_id: value
                .get("chatId")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            run_id: text("runId"),
            tool_call_id: value
                .get("toolCallId")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            tool: text("tool"),
            label: label_from_record(value),
        },
        spawned: value
            .get("spawned")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
    })
}

fn intent_dir_for(root_key: &str) -> std::path::PathBuf {
    intent_journal_dir().join(crate::fs_util::sha256(root_key.as_bytes()))
}

/// Durably publish one intent: temp file + rename, so a reader (the next
/// boot's reconciliation) sees either the whole record or none of it.
pub(crate) async fn write_intent_file(
    root_key: &str,
    intent: &ExecutionIntent,
) -> Result<(), CodingError> {
    let dir = ensure_journal_dir(root_key).await?;
    let path = dir.join(intent_file_name(&intent.identity.execution_id) + ".json");
    let temp = dir.join(format!(".tmp-{}", uuid::Uuid::new_v4().simple()));
    let written: Result<(), CodingError> = async {
        let text = serde_json::to_string(&intent_json(intent))
            .map_err(|error| CodingError::new("io_error", format!("{error}")))?;
        write_private_file(&temp, &text).await?;
        tokio::fs::rename(&temp, &path)
            .await
            .map_err(|error| CodingError::new("io_error", format!("{error}")))
    }
    .await;
    if written.is_err() {
        let _ = tokio::fs::remove_file(&temp).await;
    }
    written
}

pub(crate) async fn remove_intent_file(
    root_key: &str,
    execution_id: &str,
) -> Result<(), CodingError> {
    for dir in existing_journal_dirs(root_key).await? {
        let path = dir.join(intent_file_name(execution_id) + ".json");
        match tokio::fs::remove_file(&path).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(CodingError::new("io_error", format!("{error}"))),
        }
    }
    Ok(())
}

/// Every intent file recorded for the root. An unparseable intent is NOT
/// dropped: it reconciles as an unknown survivor set (fail closed).
pub(crate) async fn read_intent_files(root_key: &str) -> Result<Vec<ExecutionIntent>, CodingError> {
    let mut intents = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for dir in existing_journal_dirs(root_key).await? {
        read_intent_dir(&dir, root_key, &mut seen, &mut intents).await?;
    }
    Ok(intents)
}

async fn read_intent_dir(
    dir: &std::path::Path,
    root_key: &str,
    seen: &mut std::collections::HashSet<String>,
    intents: &mut Vec<ExecutionIntent>,
) -> Result<(), CodingError> {
    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(CodingError::new("io_error", format!("{error}"))),
    };
    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|error| CodingError::new("io_error", format!("{error}")))?
    {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.ends_with(".json") || name.starts_with(".tmp-") || name == QUARANTINE_FILE_NAME {
            // The root's quarantine record lives in the same journal dir;
            // it is not an intent and must never reconcile as one.
            continue;
        }
        if !seen.insert(name.clone()) {
            continue; // already read from the per-user directory
        }
        let text = tokio::fs::read_to_string(entry.path())
            .await
            .map_err(|error| CodingError::new("io_error", format!("{error}")))?;
        let intent = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|value| intent_from_json(&value))
            .unwrap_or_else(|| ExecutionIntent {
                root: root_key.to_owned(),
                identity: CommandCleanupIdentity {
                    execution_id: name.trim_end_matches(".json").replace('-', ":"),
                    owner_pid: -1,
                    pgid: None,
                    spawned_at_ms: 0,
                },
                holder: LeaseHolder {
                    chat_id: None,
                    run_id: "unknown".to_owned(),
                    tool_call_id: None,
                    tool: "unknown".to_owned(),
                    label: "unreadable execution intent".to_owned(),
                },
                spawned: false,
            });
        intents.push(intent);
    }
    Ok(())
}

/// Name of the durable quarantine record inside the root's journal
/// directory. Mirrors the lock-dir coordinator's `quarantine.json`: a
/// driver-created quarantine MUST have durable evidence, so a coordinator
/// drop (or process restart) cannot resurrect admission over an
/// unresolved cleanup. The intent journal directory is the in-process
/// coordinator's durable store — the quarantine uses the same atomic
/// temp-file + rename publication as intents, not a parallel mechanism.
pub(crate) const QUARANTINE_FILE_NAME: &str = "quarantine.json";

pub(crate) fn quarantine_json(quarantine: &WorkspaceQuarantine) -> serde_json::Value {
    serde_json::json!({
        "root": quarantine.root,
        "pids": quarantine.pids,
        "quarantinedAtMs": quarantine.quarantined_at_ms,
        "runId": quarantine.holder.run_id,
        "chatId": quarantine.holder.chat_id,
        "toolCallId": quarantine.holder.tool_call_id,
        "tool": quarantine.holder.tool,
        "labelSha256": label_digest(&quarantine.holder.label),
        "reason": quarantine.reason,
        "identity": quarantine.identity.as_ref().map(|identity| serde_json::json!({
            "executionId": identity.execution_id,
            "ownerPid": identity.owner_pid,
            "pgid": identity.pgid,
            "spawnedAtMs": identity.spawned_at_ms,
        })),
    })
}

pub(crate) fn quarantine_from_json(
    root: &str,
    value: &serde_json::Value,
) -> Option<WorkspaceQuarantine> {
    // One bad pid rejects the record (-> unknown survivors, fail closed)
    // rather than being silently dropped from the survivor set.
    // A record with NO `pids` key (older / TS-written) keeps its
    // identity-based clearing; a PRESENT but invalid list is rejected.
    let pids = match value.get("pids") {
        None => Vec::new(),
        Some(pids) => pids
            .as_array()?
            .iter()
            .map(parse_pid)
            .collect::<Option<Vec<i32>>>()?,
    };
    let text = |name: &str| {
        value
            .get(name)
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown")
            .to_owned()
    };
    // Absent / null identity = generic evidence; a PRESENT but malformed one
    // rejects the record instead of downgrading it to the weaker pid-only rule.
    let identity = match value.get("identity") {
        None | Some(serde_json::Value::Null) => None,
        Some(identity) => Some(CommandCleanupIdentity {
            execution_id: identity.get("executionId")?.as_str()?.to_owned(),
            owner_pid: i64::from(parse_pid(identity.get("ownerPid")?)?),
            pgid: match identity.get("pgid") {
                None | Some(serde_json::Value::Null) => None,
                Some(pgid) => Some(parse_pid(pgid)?),
            },
            spawned_at_ms: identity
                .get("spawnedAtMs")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or_default(),
        }),
    };
    Some(WorkspaceQuarantine {
        root: root.to_owned(),
        pids,
        holder: LeaseHolder {
            chat_id: value
                .get("chatId")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            run_id: text("runId"),
            tool_call_id: value
                .get("toolCallId")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            tool: text("tool"),
            label: label_from_record(value),
        },
        quarantined_at_ms: value
            .get("quarantinedAtMs")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default(),
        reason: value
            .get("reason")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        identity,
    })
}

/// Durably publish the root's quarantine record: temp file + rename, so a
/// reader (the next coordinator over the same store) sees either the whole
/// record or none of it. Failure is returned: an unpublished quarantine
/// must never look like success (the caller keeps the lease and the intent).
pub(crate) async fn write_quarantine_file(
    root_key: &str,
    quarantine: &WorkspaceQuarantine,
) -> Result<(), CodingError> {
    let dir = ensure_journal_dir(root_key).await?;
    let path = dir.join(QUARANTINE_FILE_NAME);
    let temp = dir.join(format!(".tmp-quarantine-{}", uuid::Uuid::new_v4().simple()));
    let written: Result<(), CodingError> = async {
        let text = serde_json::to_string(&quarantine_json(quarantine))
            .map_err(|error| CodingError::new("io_error", format!("{error}")))?;
        write_private_file(&temp, &text).await?;
        tokio::fs::rename(&temp, &path)
            .await
            .map_err(|error| CodingError::new("io_error", format!("{error}")))
    }
    .await;
    if written.is_err() {
        let _ = tokio::fs::remove_file(&temp).await;
    }
    written
}

/// Unconditional removal: explicit operator recovery only. Retirement on
/// positive evidence goes through [`retire_quarantine_file`].
pub(crate) async fn remove_quarantine_file(root_key: &str) -> Result<(), CodingError> {
    for dir in existing_journal_dirs(root_key).await? {
        let path = dir.join(QUARANTINE_FILE_NAME);
        match tokio::fs::remove_file(&path).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(CodingError::new("io_error", format!("{error}"))),
        }
    }
    Ok(())
}

/// Retire the root's durable quarantine record, but only if it is still the
/// record that was judged positively exited. Returns `true` when retired (or
/// already gone), `false` when a DIFFERENT record sits there now — the
/// caller must keep treating the root as quarantined.
pub(crate) async fn retire_quarantine_file(
    root_key: &str,
    scanned: &WorkspaceQuarantine,
) -> Result<bool, CodingError> {
    let mut all_retired = true;
    for dir in existing_journal_dirs(root_key).await? {
        let path = dir.join(QUARANTINE_FILE_NAME);
        all_retired &= retire_quarantine_record(&path, root_key, scanned).await?;
    }
    Ok(all_retired)
}

/// Do two quarantine records describe the same unresolved cleanup? Compares
/// the fields that make a record THIS record rather than a republished one:
/// publication time, execution identity, owning run / tool call, pids.
pub(crate) fn same_quarantine_record(a: &WorkspaceQuarantine, b: &WorkspaceQuarantine) -> bool {
    a.quarantined_at_ms == b.quarantined_at_ms
        && a.identity == b.identity
        && a.pids == b.pids
        && a.holder.run_id == b.holder.run_id
        && a.holder.tool_call_id == b.holder.tool_call_id
}

/// Check-then-unlink without the race: `remove_file` after a read could
/// delete a record republished in between (a NEW unresolved cleanup).
/// Instead the record is renamed to a unique tombstone — which atomically
/// pins one concrete file — re-read, compared with the scanned record, and
/// only then unlinked. A mismatch puts the record back. If the path was
/// repopulated meanwhile the newer record wins and the displaced one stays
/// in its tombstone (`<path>.retiring.<uuid>`, never read by any scanner)
/// for an operator; the result is `false` (still quarantined, fail closed).
/// Remaining window: between the rename and the restore a concurrent reader
/// sees no record; that needs a mismatch to occur at all.
///
/// The whole sequence is ONE synchronous closure on the blocking pool: a
/// spawned blocking task runs to completion even if the awaiting future is
/// dropped, so cancellation can never strand a record in a tombstone.
pub(crate) async fn retire_quarantine_record(
    path: &std::path::Path,
    root_key: &str,
    scanned: &WorkspaceQuarantine,
) -> Result<bool, CodingError> {
    let (path, root_key, scanned) = (path.to_owned(), root_key.to_owned(), scanned.clone());
    join_blocking(tokio::task::spawn_blocking(move || {
        retire_quarantine_record_sync(&path, &root_key, &scanned)
    }))
    .await
}

fn retire_quarantine_record_sync(
    path: &std::path::Path,
    root_key: &str,
    scanned: &WorkspaceQuarantine,
) -> Result<bool, CodingError> {
    let tomb = std::path::PathBuf::from(format!(
        "{}.retiring.{}",
        path.display(),
        uuid::Uuid::new_v4().simple()
    ));
    match std::fs::rename(path, &tomb) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(journal_io_error(&error)),
    }
    let moved = match std::fs::read_to_string(&tomb) {
        Ok(text) => serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|value| quarantine_from_json(root_key, &value)),
        Err(error) => {
            restore_tombstone(&tomb, path);
            return Err(journal_io_error(&error));
        }
    };
    if moved
        .as_ref()
        .is_some_and(|moved| same_quarantine_record(moved, scanned))
    {
        match std::fs::remove_file(&tomb) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
            Err(error) => Err(journal_io_error(&error)),
        }
    } else {
        restore_tombstone(&tomb, path);
        Ok(false)
    }
}

/// Put a displaced record back. `hard_link` never overwrites, so a record
/// published into the vacated path wins; ours then stays in the tombstone.
fn restore_tombstone(tomb: &std::path::Path, path: &std::path::Path) {
    match std::fs::hard_link(tomb, path) {
        Ok(()) => {
            let _ = std::fs::remove_file(tomb);
        }
        Err(error) => eprintln!(
            "Warning: quarantine record displaced by a concurrent retire kept at {} ({error}); \
             a newer record occupies {}",
            tomb.display(),
            path.display()
        ),
    }
}

/// Grace window before a `.retiring.*` tombstone becomes sweep-eligible
/// (AGENTKIT-74 hygiene): fresh tombstones belong to a retirement that
/// may still be resolving.
const RETIRING_GRACE_MS: i64 = 60 * 60 * 1_000;

/// Legacy shared-journal records age far longer before sweeping: they are
/// honored as unresolved-cleanup evidence on every read, so only records
/// that are BOTH ancient and name only dead processes are litter.
const LEGACY_JOURNAL_GRACE_MS: i64 = 7 * 24 * 60 * 60 * 1_000;

/// AGENTKIT-74 cleanup sweep for retirement tombstones. A failed
/// quarantine retirement (a republished record won the name) keeps the
/// displaced record at `<record>.retiring.<uuid>` "for an operator" —
/// nothing ever revisited them. The sweep removes tombstones past a grace
/// window whose record names only DEAD processes (evidence of a holder
/// that can no longer be contending anything); a tombstone naming a live
/// pid, an empty (explicit-recovery-blocked) pid set, or an unparseable
/// record stays, and each candidate is flocked before removal so a
/// retirement in flight is never raced.
fn sweep_retiring_tombstones_sync(
    dir: std::path::PathBuf,
    min_age_ms: i64,
) -> Vec<std::path::PathBuf> {
    let mut swept = Vec::new();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return swept;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.contains(".retiring.") || name.starts_with(".tmp-") {
            continue;
        }
        let age_ms = entry
            .metadata()
            .ok()
            .and_then(|meta| meta.modified().ok())
            .and_then(|modified| modified.elapsed().ok())
            .map_or(0, |age| age.as_millis() as i64);
        if age_ms < min_age_ms {
            continue;
        }
        let path = entry.path();
        let Ok(handle) = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
        else {
            continue;
        };
        if handle.try_lock().is_err() {
            continue; // someone else is looking at it: never race
        }
        let evidence_of_live_owner = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|value| quarantine_from_json("", &value))
                .is_some_and(|record| {
                    record.pids.is_empty()
                        || record
                            .pids
                            .iter()
                            .any(|pid| crate::process::owner_alive(i64::from(*pid)))
                }),
            // An unreadable tombstone proves nothing about who it named:
            // fail closed and keep it.
            Err(_) => true,
        };
        if evidence_of_live_owner || std::fs::remove_file(&path).is_err() {
            continue;
        }
        swept.push(path);
    }
    swept
}

/// AGENTKIT-74 cleanup sweep for the world-shared legacy journal
/// directory (`$TMP/agentkit-exec-intents/`, superseded by the per-user
/// `agentkit-exec-intents-<uid>/`). Records there are honored on read
/// forever, but an abandoned root's records — every pid they name dead,
/// the leaf older than the grace window — are litter in a shared temp
/// directory. Leaf-granular and fail-closed: a leaf is removed only when
/// EVERY record in it parses to a non-empty pid set that is entirely
/// dead; any live pid, any empty (explicit-recovery-blocked) quarantine,
/// any unreadable or unparseable record, keeps the whole leaf. Only a
/// real directory owned by this user is ever entered or removed.
#[cfg(unix)]
fn sweep_legacy_journal(min_age_ms: i64) {
    use std::os::unix::fs::MetadataExt;
    let base = std::env::temp_dir().join("agentkit-exec-intents");
    let Ok(meta) = std::fs::symlink_metadata(&base) else {
        return;
    };
    if !meta.file_type().is_dir() || meta.uid() != current_uid() {
        return; // foreign or not-a-directory: never touched (as on read)
    }
    let Ok(leaves) = std::fs::read_dir(&base) else {
        return;
    };
    for leaf in leaves.flatten() {
        let Ok(meta) = leaf.metadata() else {
            continue;
        };
        if !meta.file_type().is_dir() || meta.uid() != current_uid() {
            continue;
        }
        let age_ms = meta
            .modified()
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .map_or(0, |age| age.as_millis() as i64);
        if age_ms < min_age_ms {
            continue;
        }
        let dir = leaf.path();
        let Ok(files) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut doomed = Vec::new();
        let mut keep_leaf = false;
        for file in files.flatten() {
            let name = file.file_name().to_string_lossy().into_owned();
            if name.starts_with(".tmp-") || !name.ends_with(".json") {
                continue;
            }
            let path = file.path();
            let pids: Option<Vec<i64>> = match std::fs::read_to_string(&path)
                .ok()
                .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
            {
                None => None,
                Some(value) if name == QUARANTINE_FILE_NAME => quarantine_from_json("", &value)
                    .map(|record| record.pids.iter().map(|pid| i64::from(*pid)).collect()),
                Some(value) => value
                    .get("ownerPid")
                    .and_then(|pid| pid.as_i64())
                    // A corrupt (absurd/absent) owner pid is the unknown-
                    // survivor kind on read: fail closed, keep the leaf.
                    .filter(|pid| *pid > 0)
                    .map(|pid| vec![pid]),
            };
            let dead_set = pids.is_some_and(|pids| {
                !pids.is_empty() && pids.iter().all(|pid| !crate::process::owner_alive(*pid))
            });
            if !dead_set {
                keep_leaf = true;
                break;
            }
            doomed.push(path);
        }
        if keep_leaf || doomed.is_empty() {
            continue;
        }
        for path in doomed {
            let _ = std::fs::remove_file(&path);
        }
        let _ = std::fs::remove_dir(&dir); // only succeeds when now empty
    }
}

#[cfg(not(unix))]
fn sweep_legacy_journal(_min_age_ms: i64) {}

/// Read the root's durable quarantine record. An unparseable record is NOT
/// dropped: it reconciles as an unknown-survivor quarantine (empty pids
/// never "positively exit"), fail closed until explicit recovery.
pub(crate) async fn read_quarantine_file(
    root_key: &str,
) -> Result<Option<WorkspaceQuarantine>, CodingError> {
    // The per-user directory wins; a legacy record is honored when it has
    // no per-user successor.
    let mut found = None;
    for dir in existing_journal_dirs(root_key).await? {
        match tokio::fs::read_to_string(dir.join(QUARANTINE_FILE_NAME)).await {
            Ok(text) => {
                found = Some(text);
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(CodingError::new("io_error", format!("{error}"))),
        }
    }
    let Some(text) = found else {
        return Ok(None);
    };
    let quarantine = serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|value| quarantine_from_json(root_key, &value))
        .unwrap_or_else(|| WorkspaceQuarantine {
            root: root_key.to_owned(),
            pids: vec![],
            holder: LeaseHolder {
                chat_id: None,
                run_id: "unknown".to_owned(),
                tool_call_id: None,
                tool: "unknown".to_owned(),
                label: "unreadable quarantine record".to_owned(),
            },
            quarantined_at_ms: 0,
            reason: None,
            identity: None,
        });
    Ok(Some(quarantine))
}

impl InProcessCoordinator {
    pub fn new(observer: Option<Arc<dyn CoordinatorObserver>>) -> Self {
        Self {
            workspaces: Arc::new(GateMap::new()),
            files: Arc::new(GateMap::new()),
            observer,
            quarantines: Arc::new(Mutex::new(HashMap::new())),
            intents: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// The live quarantine for the root: the in-memory record first, then
    /// the DURABLE journal record (a driver-created quarantine must survive
    /// a coordinator drop / process restart — memory alone was the B-durability
    /// defect: the next coordinator over the same store admitted modifying
    /// work). A journal read error is returned, never swallowed into
    /// "nothing quarantined": fail closed.
    async fn live_quarantine(
        &self,
        root: &str,
    ) -> Result<Option<WorkspaceQuarantine>, CodingError> {
        let candidate = {
            let quarantines = self
                .quarantines
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            quarantines.get(root).cloned()
        };
        let candidate = match candidate {
            Some(candidate) => candidate,
            None => match read_quarantine_file(root).await? {
                Some(candidate) => candidate,
                None => return Ok(None),
            },
        };
        if quarantine_positively_exited(&candidate).await {
            // Positive cleanup evidence: every recorded process exited.
            // Retire BOTH halves of the record. A failed durable removal is
            // an `Err` (the root stays blocked) — half-retiring would let a
            // reopen resurrect the same uncertain evidence anyway.
            if !retire_quarantine_file(root, &candidate).await? {
                // A different durable record replaced the one judged exited.
                // Drop a stale in-memory mirror of the judged one (else the
                // root stays blocked on it forever), then report the durable
                // record that now governs the root.
                {
                    let mut quarantines = self
                        .quarantines
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner);
                    if quarantines
                        .get(root)
                        .is_some_and(|current| same_quarantine_record(current, &candidate))
                    {
                        quarantines.remove(root);
                    }
                }
                return read_quarantine_file(root).await;
            }
            let mut quarantines = self
                .quarantines
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if let Some(current) = quarantines.get(root)
                && current.pids == candidate.pids
                && current.identity == candidate.identity
            {
                quarantines.remove(root);
            }
            return Ok(None);
        }
        // Hydrate the in-memory mirror so the wait loop does not re-read the
        // journal on every poll.
        self.quarantines
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(root.to_owned())
            .or_insert_with(|| candidate.clone());
        Ok(Some(candidate))
    }

    /// Block until the root carries no live quarantine, the caller cancels,
    /// or the acquisition's wait budget expires. Unlike gate waits this one
    /// keeps its own deadline so the surfaced error stays
    /// `workspace_quarantined` rather than a generic busy.
    async fn await_clear(
        &self,
        root: &str,
        cancellation: &Option<CancellationToken>,
        max_wait_ms: Option<u64>,
    ) -> Result<(), CodingError> {
        let deadline =
            max_wait_ms.map(|at| std::time::Instant::now() + std::time::Duration::from_millis(at));
        loop {
            let Some(quarantine) = self.live_quarantine(root).await? else {
                return Ok(());
            };
            if deadline.is_some_and(|at| std::time::Instant::now() >= at) {
                return Err(quarantine_error(&quarantine));
            }
            let Some(token) = cancellation else {
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                continue;
            };
            tokio::select! {
                _ = token.cancelled() => {
                    // The wait budget (or the caller) ended while an
                    // unresolved cleanup still blocks the root: surface the
                    // quarantine, not a generic abort/busy.
                    return Err(match self.live_quarantine(root).await? {
                        Some(quarantine) => quarantine_error(&quarantine),
                        None => abort_error(),
                    });
                }
                _ = tokio::time::sleep(std::time::Duration::from_millis(25)) => {}
            }
        }
    }

    /// Refuse an executable lease the moment a live quarantine names the root.
    ///
    /// [`Self::await_clear`] can only prove the root clear BEFORE the gate is
    /// taken: a quarantine published while this acquisition sat in the queue
    /// would otherwise be handed a writable lease (defect B2 — a queued writer
    /// slipping past live unresolved-cleanup evidence). Calling this under the
    /// freshly granted gate(s) closes that window: the quarantining caller
    /// holds its own lease until publication returns `Ok`, so the record is
    /// already visible while the gate is still held by somebody, and no new
    /// grant can complete between this check and the lease being returned.
    async fn quarantine_refusal(&self, root: &str) -> Result<Option<CodingError>, CodingError> {
        Ok(self
            .live_quarantine(root)
            .await?
            .as_ref()
            .map(quarantine_error))
    }

    #[allow(clippy::too_many_arguments)] // one acquisition, all inputs explicit
    async fn acquire_single(
        &self,
        map: &Arc<GateMap>,
        key: String,
        mode: Mode,
        kind: WaitKind,
        root: &str,
        path: Option<&str>,
        holder: &LeaseHolder,
        cancellation: &Option<CancellationToken>,
        tracker: &WaitTracker,
    ) -> Result<ReleaseFn, CodingError> {
        let observer = self.observer.clone();
        let state = map.try_acquire(&key, mode, holder);
        let tracker = tracker.clone();
        let root_owned = root.to_owned();
        let path_owned = path.map(str::to_owned);
        let holder_owned = holder.clone();
        let id = map
            .wait(&key, state, cancellation, &move |holders| {
                tracker.waiting(
                    CoordinationWait {
                        kind,
                        root: root_owned.clone(),
                        path: path_owned.clone(),
                        waiter: holder_owned.clone(),
                        holders: holders.to_vec(),
                    },
                    observer.as_ref(),
                );
            })
            .await?;
        Ok(sync_release(Arc::clone(map), key, id))
    }

    pub(crate) async fn acquire_file_tracked(
        &self,
        root: &str,
        path: &str,
        holder: &LeaseHolder,
        cancellation: &Option<CancellationToken>,
        tracker: &WaitTracker,
        quarantine_wait_ms: Option<u64>,
    ) -> Result<WorkspaceLease, CodingError> {
        let root = canonical_root_key(root).await;
        self.await_clear(&root, cancellation, quarantine_wait_ms)
            .await?;
        let path = canonical_file_key(path).await;
        let mut release_shared = PendingRelease::new(
            self.acquire_single(
                &self.workspaces,
                root.clone(),
                Mode::Shared,
                WaitKind::Workspace,
                &root,
                None,
                holder,
                cancellation,
                tracker,
            )
            .await?,
        );
        match self
            .acquire_single(
                &self.files,
                path.clone(),
                Mode::Exclusive,
                WaitKind::File,
                &root,
                Some(&path),
                holder,
                cancellation,
                tracker,
            )
            .await
        {
            Ok(release_file) => {
                let mut lease = WorkspaceLease::new(vec![release_file, release_shared.consume()]);
                // B2: both gates are held now, so this is the last point at
                // which an evidence record that landed while we queued can
                // still stop us. Never return an executable lease over live
                // unresolved-cleanup evidence.
                if let Some(error) = self.quarantine_refusal(&root).await? {
                    lease.release().await;
                    return Err(error);
                }
                Ok(lease)
            }
            Err(error) => {
                if let Some(release) = release_shared.take() {
                    release().await;
                }
                Err(error)
            }
        }
    }

    pub(crate) async fn acquire_workspace_tracked(
        &self,
        root: &str,
        holder: &LeaseHolder,
        cancellation: &Option<CancellationToken>,
        tracker: &WaitTracker,
        quarantine_wait_ms: Option<u64>,
    ) -> Result<WorkspaceLease, CodingError> {
        let root = canonical_root_key(root).await;
        self.await_clear(&root, cancellation, quarantine_wait_ms)
            .await?;
        let release = self
            .acquire_single(
                &self.workspaces,
                root.clone(),
                Mode::Exclusive,
                WaitKind::Workspace,
                &root,
                None,
                holder,
                cancellation,
                tracker,
            )
            .await?;
        let mut lease = WorkspaceLease::new(vec![release]);
        // B2: see the file-lease path — the exclusive gate is ours now, so a
        // quarantine that landed while we queued still stops us here rather
        // than after we have written to the workspace.
        if let Some(error) = self.quarantine_refusal(&root).await? {
            lease.release().await;
            return Err(error);
        }
        Ok(lease)
    }
}

#[async_trait]
impl WorkspaceCoordinator for InProcessCoordinator {
    async fn acquire_file(&self, input: AcquireFileInput) -> Result<WorkspaceLease, CodingError> {
        // B5: no modifying work is admitted before the once-per-process
        // startup sweep completed (awaited, never spawned-and-forgotten)
        // and this root's crashed-run intents have been reconciled.
        crate::process::startup_recovery().await;
        self.reconcile_execution_intents(&input.root).await?;
        let tracker = WaitTracker::default();
        let observer = self.observer.clone();
        let holder = input.holder.clone();
        let root = input.root.clone();
        let path = input.path.clone();
        let this = self.clone();
        let tracked_tracker = tracker.clone();
        let quarantine_ms = input.max_wait_ms;
        let result = with_wait_cap(
            input.cancellation.clone(),
            input.max_wait_ms,
            &tracker,
            move |cancellation| {
                let this = this.clone();
                let tracker = tracked_tracker.clone();
                let root = root.clone();
                let path = path.clone();
                let holder = holder.clone();
                Box::pin(async move {
                    this.acquire_file_tracked(
                        &root,
                        &path,
                        &holder,
                        &cancellation,
                        &tracker,
                        quarantine_ms,
                    )
                    .await
                })
            },
        )
        .await?;
        tracker.acquired(observer.as_ref());
        Ok(result)
    }

    async fn acquire_workspace(
        &self,
        input: AcquireWorkspaceInput,
    ) -> Result<WorkspaceLease, CodingError> {
        // B5: startup recovery is awaited here, never spawned and
        // forgotten; crashed-run intents for this root are reconciled
        // before any modifying work may be admitted.
        crate::process::startup_recovery().await;
        self.reconcile_execution_intents(&input.root).await?;
        let tracker = WaitTracker::default();
        let observer = self.observer.clone();
        let holder = input.holder.clone();
        let root = input.root.clone();
        let this = self.clone();
        let tracked_tracker = tracker.clone();
        let quarantine_ms = input.max_wait_ms;
        let result = with_wait_cap(
            input.cancellation.clone(),
            input.max_wait_ms,
            &tracker,
            move |cancellation| {
                let this = this.clone();
                let tracker = tracked_tracker.clone();
                let root = root.clone();
                let holder = holder.clone();
                Box::pin(async move {
                    this.acquire_workspace_tracked(
                        &root,
                        &holder,
                        &cancellation,
                        &tracker,
                        quarantine_ms,
                    )
                    .await
                })
            },
        )
        .await?;
        tracker.acquired(observer.as_ref());
        Ok(result)
    }

    async fn quarantine_workspace(
        &self,
        root: &str,
        evidence: &QuarantineEvidence,
    ) -> Result<(), CodingError> {
        let root = canonical_root_key(root).await;
        let record = WorkspaceQuarantine {
            root: root.clone(),
            pids: evidence.pids.clone(),
            holder: evidence.holder.clone(),
            quarantined_at_ms: now_ms_u64(),
            reason: evidence.reason.clone(),
            identity: None,
        };
        self.quarantines
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(root.clone(), record.clone());
        // Durability rule: the in-memory record alone dies with the
        // coordinator, so the durable journal publication IS the blocking
        // evidence across a reopen. Its failure is returned (the in-memory
        // block is kept too — fail closed): the caller must retain the
        // modifying lease and treat the root as blocked.
        write_quarantine_file(&root, &record).await
    }

    async fn workspace_quarantine(
        &self,
        root: &str,
    ) -> Result<Option<WorkspaceQuarantine>, CodingError> {
        let root = canonical_root_key(root).await;
        self.live_quarantine(&root).await
    }

    async fn clear_workspace_quarantine(&self, root: &str) -> Result<bool, CodingError> {
        let root = canonical_root_key(root).await;
        // Explicit recovery retires BOTH halves of the record, durable
        // first: if the journal removal fails the whole recovery returns
        // `Err` with everything still in place — a half-cleared root
        // (memory gone, durable record intact) would just re-block on the
        // next acquisition and lie about the recovery having happened.
        let had_durable = read_quarantine_file(&root).await?.is_some();
        remove_quarantine_file(&root).await?;
        // Evaluate BOTH halves unconditionally (`||` would short-circuit
        // the memory removal whenever a durable record existed).
        let removed_memory = self
            .quarantines
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&root)
            .is_some();
        let removed = had_durable || removed_memory;
        // Explicit recovery retires the DURABLE unresolved-execution
        // evidence too. While a quarantine blocks, reconciliation keeps the
        // intent record (the in-memory quarantine alone does not survive a
        // drop); dropping only the in-memory half would have the next
        // acquisition re-quarantine the root this recovery was asked to
        // release. Retire every DEAD-owner intent for the root — a live
        // owner's intent belongs to an in-flight execution, not to this
        // recovery. A failed retirement is returned: the root then stays
        // blocked (fail closed) rather than half-recovered.
        let mut candidates: Vec<ExecutionIntent> = self
            .intents
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&root)
            .map(|live| live.values().cloned().collect())
            .unwrap_or_default();
        candidates.extend(read_intent_files(&root).await?);
        let own_pid = i64::from(std::process::id());
        let mut failure: Option<CodingError> = None;
        for intent in candidates {
            let owner_alive = intent.identity.owner_pid > 0
                && (intent.identity.owner_pid == own_pid
                    || crate::process::owner_alive(intent.identity.owner_pid));
            if owner_alive {
                continue;
            }
            if let Err(error) = self
                .clear_execution_intent(&root, &intent.identity.execution_id)
                .await
            {
                failure.get_or_insert(error);
            }
        }
        match failure {
            Some(error) => Err(error),
            None => Ok(removed),
        }
    }

    async fn quarantine_execution(
        &self,
        root: &str,
        evidence: &QuarantineEvidence,
        identity: &CommandCleanupIdentity,
    ) -> Result<(), CodingError> {
        let root = canonical_root_key(root).await;
        let record = WorkspaceQuarantine {
            root: root.clone(),
            pids: evidence.pids.clone(),
            holder: evidence.holder.clone(),
            quarantined_at_ms: now_ms_u64(),
            reason: evidence.reason.clone(),
            identity: Some(identity.clone()),
        };
        self.quarantines
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(root.clone(), record.clone());
        // Driver-created quarantine must be DURABLE (the assessor defect:
        // a memory-only Ok here let the driver delete the durable intent,
        // so a coordinator reopen admitted modifying work over a live
        // marked child). Publication failure is `Err`: the driver keeps
        // the lease AND the intent — the root never ends up with neither.
        write_quarantine_file(&root, &record).await
    }

    async fn record_execution_intent(&self, intent: &ExecutionIntent) -> Result<(), CodingError> {
        let root = canonical_root_key(&intent.root).await;
        let stored = ExecutionIntent {
            root: root.clone(),
            ..intent.clone()
        };
        self.intents
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(root.clone())
            .or_default()
            .insert(stored.identity.execution_id.clone(), stored.clone());
        write_intent_file(&root, &stored).await
    }

    async fn clear_execution_intent(
        &self,
        root: &str,
        execution_id: &str,
    ) -> Result<(), CodingError> {
        let root = canonical_root_key(root).await;
        // The first guard MUST be dropped before re-locking: holding a guard
        // across a nested `lock()` of the same std mutex self-deadlocks the
        // thread (single-threaded runtimes hang forever).
        let drained = self
            .intents
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_mut(&root)
            .map(|root_intents| {
                root_intents.remove(execution_id);
                root_intents.is_empty()
            })
            .unwrap_or(false);
        if drained {
            self.intents
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&root);
        }
        remove_intent_file(&root, execution_id).await
    }

    async fn reconcile_execution_intents(&self, root: &str) -> Result<(), CodingError> {
        let root = canonical_root_key(root).await;
        let mut candidates: Vec<ExecutionIntent> = self
            .intents
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&root)
            .map(|live| live.values().cloned().collect())
            .unwrap_or_default();
        candidates.extend(read_intent_files(&root).await?);
        let own_pid = i64::from(std::process::id());
        for intent in candidates {
            let owner_alive = intent.identity.owner_pid > 0
                && (intent.identity.owner_pid == own_pid
                    || crate::process::owner_alive(intent.identity.owner_pid));
            if owner_alive {
                // Owner still running in this or another live process: the
                // intent belongs to an in-flight execution, not a crash.
                // An absurd/zero pid (a corrupt record) is never signal-
                // probed; it reconciles as unknown.
                continue;
            }
            // A corrupt/unreadable intent (absurd owner pid) proves nothing
            // about cleanup: classify it as UNKNOWN survivors so the root
            // blocks until explicit recovery — never retire it as if the
            // process group had positively exited.
            let corrupt = intent.identity.owner_pid <= 0;
            let survivors = if corrupt {
                None
            } else {
                crate::process::crashed_execution_survivors(&intent).await
            };
            if let Some(survivors) = survivors.as_ref()
                && survivors.is_empty()
            {
                // Positive evidence the process group is gone: retire. A
                // failed retirement is an `Err`, not a swallowed success:
                // the next acquisition re-reconciles and retries.
                self.clear_execution_intent(&root, &intent.identity.execution_id)
                    .await?;
                continue;
            }
            // Unknown (None) or live survivors: block the root with the
            // intent's SAFE identity. A corrupt record carries no
            // trustworthy identity, so its quarantine is the generic
            // unknown-survivor kind (empty pids stay blocked).
            //
            // Durability rule (mirrors the driver's "retire the intent only
            // once its replacement exists" in `spawn_owned_execution`):
            // this quarantine lives in MEMORY only, so the durable intent
            // record is the ONLY evidence that survives a coordinator drop
            // or process exit. The intent is therefore NEVER retired while
            // it still blocks: only positive group-exit (above) or explicit
            // recovery (`clear_workspace_quarantine`, which retires the
            // dead-owner intents alongside the quarantine) may remove it.
            // Retiring it here would leave the root with neither intent nor
            // durable quarantine evidence while the unresolved execution
            // remains — the next coordinator would admit modifying work.
            let pids = survivors.unwrap_or_default();
            if self.live_quarantine(&root).await?.is_none() {
                self.quarantines
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(
                        root.clone(),
                        WorkspaceQuarantine {
                            root: root.clone(),
                            pids,
                            holder: intent.holder.clone(),
                            quarantined_at_ms: now_ms_u64(),
                            reason: None,
                            identity: (!corrupt).then(|| intent.identity.clone()),
                        },
                    );
            }
            // If a live quarantine already blocks the root it was published
            // for a DIFFERENT execution's identity, so it does not represent
            // this intent: keep the intent so its identity is never
            // discarded, and so a later reopen can still see it.
        }
        // AGENTKIT-74 cleanup sweeps (best-effort hygiene; correctness
        // never depends on them): retirement tombstones this root's
        // journal leaves kept for an operator, and abandoned records in
        // the world-shared legacy journal directory, are swept only once
        // every pid they name is provably gone (see the sweep docs).
        if let Ok(dirs) = existing_journal_dirs(&root).await {
            for dir in dirs {
                sweep_retiring_tombstones_sync(dir, RETIRING_GRACE_MS);
            }
        }
        static LEGACY_SWEEPED: std::sync::Once = std::sync::Once::new();
        LEGACY_SWEEPED.call_once(|| sweep_legacy_journal(LEGACY_JOURNAL_GRACE_MS));
        Ok(())
    }
}

pub fn create_in_process_workspace_coordinator(
    observer: Option<Arc<dyn CoordinatorObserver>>,
) -> Arc<dyn WorkspaceCoordinator> {
    Arc::new(InProcessCoordinator::new(observer))
}

static SHARED: LazyLock<Arc<InProcessCoordinator>> =
    LazyLock::new(|| Arc::new(InProcessCoordinator::new(None)));

/// The one coordinator every pack in this process uses unless told
/// otherwise. Lock-dir coordinators also take their in-process leases from
/// it, so packs on either kind exclude each other.
pub fn default_in_process_coordinator() -> Arc<InProcessCoordinator> {
    Arc::clone(&SHARED)
}

pub fn default_workspace_coordinator() -> Arc<dyn WorkspaceCoordinator> {
    default_in_process_coordinator()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn holder(id: &str, tool: &str, label: &str) -> LeaseHolder {
        LeaseHolder {
            chat_id: Some("chat".into()),
            run_id: id.into(),
            tool_call_id: None,
            tool: tool.into(),
            label: label.into(),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn quarantine_blocks_writers_until_survivors_exit() {
        let coordinator = InProcessCoordinator::new(None);
        let root = temp_dir("quarantine-root");
        let mut child = tokio::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id().expect("pid") as i32;
        coordinator
            .quarantine_workspace(
                &root,
                &QuarantineEvidence {
                    pids: vec![pid],
                    holder: holder("r1", "command", "long build"),
                    reason: None,
                },
            )
            .await
            .expect("blocking evidence publishes");
        let error = coordinator
            .acquire_workspace(AcquireWorkspaceInput {
                root: root.clone(),
                holder: holder("r2", "command", "next"),
                cancellation: None,
                max_wait_ms: Some(120),
                execution_id: None,
            })
            .await
            .expect_err("quarantined workspace blocks commands");
        assert_eq!(error.code, "workspace_quarantined");
        assert!(error.message.contains("long build"), "{error:?}");
        // The edit path (file lease) is blocked by the same evidence.
        let file_error = coordinator
            .acquire_file(AcquireFileInput {
                root: root.clone(),
                path: format!("{root}/f.txt"),
                holder: holder("r3", "edit_file", "edit f"),
                cancellation: None,
                max_wait_ms: Some(120),
                execution_id: None,
            })
            .await
            .expect_err("quarantined workspace blocks edits too");
        assert_eq!(file_error.code, "workspace_quarantined");
        // Positive cleanup evidence: the survivors exited.
        let _ = child.kill().await;
        let _ = child.wait().await;
        assert!(
            coordinator
                .workspace_quarantine(&root)
                .await
                .unwrap()
                .is_none()
        );
        let mut lease = coordinator
            .acquire_workspace(AcquireWorkspaceInput {
                root: root.clone(),
                holder: holder("r4", "command", "next"),
                cancellation: None,
                max_wait_ms: Some(1_000),
                execution_id: None,
            })
            .await
            .expect("clean after survivors exit");
        lease.release().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unknown_survivors_block_until_explicit_recovery() {
        let coordinator = InProcessCoordinator::new(None);
        let root = temp_dir("quarantine-unknown");
        // An empty pid list is an *unknown* survivor set (e.g. an unreadable
        // durable record): it never auto-clears.
        coordinator
            .quarantine_workspace(
                &root,
                &QuarantineEvidence {
                    pids: vec![],
                    holder: holder("r1", "command", "crashed"),
                    reason: None,
                },
            )
            .await
            .expect("unknown survivors publish as blocking evidence");
        assert!(
            coordinator
                .workspace_quarantine(&root)
                .await
                .unwrap()
                .is_some()
        );
        let error = coordinator
            .acquire_workspace(AcquireWorkspaceInput {
                root: root.clone(),
                holder: holder("r2", "command", "next"),
                cancellation: None,
                max_wait_ms: Some(80),
                execution_id: None,
            })
            .await
            .expect_err("unknown survivors keep blocking");
        assert_eq!(error.code, "workspace_quarantined");
        // Explicit recovery from the unresolved state.
        assert!(coordinator.clear_workspace_quarantine(&root).await.unwrap());
        assert!(!coordinator.clear_workspace_quarantine(&root).await.unwrap());
        let mut lease = coordinator
            .acquire_workspace(AcquireWorkspaceInput {
                root,
                holder: holder("r3", "command", "next"),
                cancellation: None,
                max_wait_ms: Some(1_000),
                execution_id: None,
            })
            .await
            .expect("clean after recovery");
        lease.release().await;
    }

    /// B5 end-to-end: admit (durable pre-spawn intent) → effect (the
    /// command writes into the workspace) → completion never arrives (the
    /// owner is SIGKILLed mid-run). A freshly built coordinator — the
    /// reopened-process perspective — must block every later modifying
    /// acquire while the command's process group is still alive, and let
    /// work through only on positive group-exit evidence.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn a_crashed_command_owner_blocks_the_workspace_until_its_group_dies() {
        use std::os::unix::process::CommandExt;
        let root = temp_dir("crash-intent");
        let execution_id = format!("{}:crash-{}", std::process::id(), next_entry_id());
        let survivor_path = std::path::Path::new(&root).join("survivor.pid");
        // The command: a survivor sibling in its own process group that
        // carries the execution marker, plus the owner itself sleeping.
        let mut owner = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(format!(
                "touch written.effect & sleep 60 & echo $! > {}; exec sleep 60",
                survivor_path.display()
            ))
            .env(crate::process_tree::EXEC_ID_ENV, &execution_id)
            .current_dir(&root)
            .process_group(0)
            .spawn()
            .expect("spawn the command");
        let owner_pid = owner.id() as i64;
        let survivor_pid: i32 = {
            // Admit: the durable intent is written BEFORE any effect starts.
            let coordinator = InProcessCoordinator::new(None);
            coordinator
                .record_execution_intent(&ExecutionIntent {
                    root: root.clone(),
                    identity: CommandCleanupIdentity {
                        execution_id: execution_id.clone(),
                        owner_pid,
                        pgid: Some(owner_pid as i32),
                        spawned_at_ms: now_ms_u64(),
                    },
                    holder: holder("cmd", "run_command", "crash"),
                    spawned: true,
                })
                .await
                .unwrap();
            // Wait for the effect and the survivor pid, then kill the owner
            // mid-run: no teardown ever runs (ADR 0020 crash window).
            let written = std::path::Path::new(&root).join("written.effect");
            tokio::time::timeout(Duration::from_secs(10), async {
                while !written.exists() || !survivor_path.exists() {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("the effect landed");
            std::fs::read_to_string(&survivor_path)
                .unwrap()
                .trim()
                .parse()
                .expect("survivor pid")
        };
        unsafe { libc::kill(owner_pid as i32, libc::SIGKILL) };
        let _ = owner.wait();

        // Reopen: a fresh instance reconciles the durable journal and finds
        // the dead owner's group still populated.
        let reopened = InProcessCoordinator::new(None);
        reopened
            .reconcile_execution_intents(&root)
            .await
            .expect("reconcile");
        let error = reopened
            .acquire_workspace(AcquireWorkspaceInput {
                root: root.clone(),
                holder: holder("cmd2", "run_command", "next"),
                cancellation: None,
                max_wait_ms: Some(80),
                execution_id: None,
            })
            .await
            .expect_err("a live crashed-command group keeps the workspace blocked");
        assert_eq!(error.code, "workspace_quarantined");
        // The quarantine names the exact execution (safe identity), not a
        // bare pid guess.
        let quarantine = reopened.workspace_quarantine(&root).await.unwrap();
        assert!(quarantine.is_some(), "blocking evidence persists");
        assert_eq!(
            quarantine
                .unwrap()
                .identity
                .expect("command identity")
                .execution_id,
            execution_id
        );

        // Positive evidence: the whole recorded group is gone — the block
        // lifts on its own, no operator recovery needed.
        unsafe { libc::kill(survivor_pid, libc::SIGKILL) };
        let mut lease = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match reopened
                    .acquire_workspace(AcquireWorkspaceInput {
                        root: root.clone(),
                        holder: holder("cmd3", "run_command", "after"),
                        cancellation: None,
                        max_wait_ms: Some(80),
                        execution_id: None,
                    })
                    .await
                {
                    Ok(lease) => return lease,
                    Err(error) if error.code == "workspace_quarantined" => {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                    Err(error) => panic!("unexpected block: {error:?}"),
                }
            }
        })
        .await
        .expect("the workspace frees once the group positively exits");
        lease.release().await;
    }

    fn temp_dir(name: &str) -> String {
        let path = std::env::temp_dir().join(format!(
            "ak-coord-{}-{name}-{}",
            std::process::id(),
            next_entry_id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path.to_string_lossy().into_owned()
    }

    struct RecordingObserver {
        events: Mutex<Vec<String>>,
        signal: tokio::sync::Notify,
    }

    impl RecordingObserver {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                events: Mutex::new(vec![]),
                signal: tokio::sync::Notify::new(),
            })
        }

        /// Barrier: register for the wakeup before re-reading the log so a
        /// notify can never land in the gap, with a poll fallback and a hard
        /// cap so a missing event fails the test instead of hanging it. The
        /// 25ms poll is a liveness net, never the thing a test relies on to
        /// order two actors.
        async fn wait_for(&self, needle: &str) {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            loop {
                if self
                    .events
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .iter()
                    .any(|event| event.contains(needle))
                {
                    return;
                }
                let notified = self.signal.notified();
                tokio::pin!(notified);
                tokio::select! {
                    () = &mut notified => {}
                    () = tokio::time::sleep(Duration::from_millis(25)) => {}
                    () = tokio::time::sleep_until(deadline) => {
                        panic!("observer never recorded {needle:?}");
                    }
                }
            }
        }
    }

    impl CoordinatorObserver for RecordingObserver {
        fn on_waiting(&self, wait: &CoordinationWait) {
            let kind = match wait.kind {
                WaitKind::File => "file",
                WaitKind::Workspace => "workspace",
            };
            self.events
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(format!(
                    "waiting:{kind}:{}:{}",
                    wait.waiter.run_id, wait.waiter.tool
                ));
            self.signal.notify_waiters();
        }

        fn on_acquired(&self, wait: &CoordinationWait) {
            self.events
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(format!(
                    "acquired:{}:{}",
                    wait.waiter.run_id, wait.waiter.tool
                ));
            self.signal.notify_waiters();
        }
    }

    fn coordinator() -> Arc<InProcessCoordinator> {
        Arc::new(InProcessCoordinator::new(None))
    }

    fn coordinator_with(observer: Arc<dyn CoordinatorObserver>) -> Arc<InProcessCoordinator> {
        Arc::new(InProcessCoordinator::new(Some(observer)))
    }

    fn file_input(root: &str, path: &str, run: &str, tool: &str, label: &str) -> AcquireFileInput {
        AcquireFileInput {
            root: root.into(),
            path: path.into(),
            holder: holder(run, tool, label),
            cancellation: None,
            max_wait_ms: None,
            execution_id: None,
        }
    }

    fn ws_input(root: &str, run: &str, tool: &str, label: &str) -> AcquireWorkspaceInput {
        AcquireWorkspaceInput {
            root: root.into(),
            holder: holder(run, tool, label),
            cancellation: None,
            max_wait_ms: None,
            execution_id: None,
        }
    }

    #[tokio::test]
    async fn distinct_files_overlap() {
        let coordinator = coordinator();
        let root = temp_dir("overlap");
        let mut lease_a = coordinator
            .acquire_file(file_input(
                &root,
                &format!("{root}/a.txt"),
                "run-a",
                "edit_file",
                "a.txt",
            ))
            .await
            .unwrap();
        let mut lease_b = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            coordinator.acquire_file(file_input(
                &root,
                &format!("{root}/b.txt"),
                "run-b",
                "write_file",
                "b.txt",
            )),
        )
        .await
        .expect("distinct files run concurrently")
        .unwrap();
        lease_a.release().await;
        lease_b.release().await;
        lease_b.release().await; // idempotent
    }

    #[tokio::test]
    async fn same_file_serializes() {
        let observer = RecordingObserver::new();
        let coordinator = coordinator_with(observer.clone());
        let root = temp_dir("same-file");
        let file = format!("{root}/a.txt");
        let mut lease_a = coordinator
            .acquire_file(file_input(&root, &file, "run-a", "edit_file", "a.txt"))
            .await
            .unwrap();
        let waiter = {
            let coordinator = Arc::clone(&coordinator);
            let root = root.clone();
            let file = file.clone();
            tokio::spawn(async move {
                coordinator
                    .acquire_file(file_input(&root, &file, "run-b", "edit_file", "a.txt"))
                    .await
            })
        };
        observer.wait_for("waiting:file:run-b:edit_file").await;
        let started = std::time::Instant::now();
        while !waiter.is_finished() && started.elapsed() < std::time::Duration::from_millis(120) {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(!waiter.is_finished(), "same-file lease must block");
        lease_a.release().await;
        let mut lease_b = tokio::time::timeout(std::time::Duration::from_secs(2), waiter)
            .await
            .expect("granted after release")
            .expect("join")
            .expect("acquire");
        observer.wait_for("acquired:run-b:edit_file").await;
        lease_b.release().await;
    }

    #[tokio::test]
    async fn writer_preference_blocks_overtaking_readers() {
        let observer = RecordingObserver::new();
        let coordinator = coordinator_with(observer.clone());
        let root = temp_dir("writer-pref");
        let file_a = format!("{root}/a.txt");
        let file_b = format!("{root}/b.txt");
        let mut lease_a = coordinator
            .acquire_file(file_input(&root, &file_a, "run-a", "edit_file", "a.txt"))
            .await
            .unwrap();
        // B queues for the exclusive workspace lease behind A's shared one.
        let b = {
            let coordinator = Arc::clone(&coordinator);
            let root = root.clone();
            tokio::spawn(async move {
                coordinator
                    .acquire_workspace(AcquireWorkspaceInput {
                        root,
                        holder: holder("run-b", "run_command", "make test"),
                        cancellation: None,
                        max_wait_ms: None,
                        execution_id: None,
                    })
                    .await
            })
        };
        observer
            .wait_for("waiting:workspace:run-b:run_command")
            .await;
        // C requests a DIFFERENT file lease after B queued: writer
        // preference must hold C behind B.
        let mut c = {
            let coordinator = Arc::clone(&coordinator);
            let root = root.clone();
            let path = file_b.clone();
            tokio::spawn(async move {
                coordinator
                    .acquire_file(file_input(&root, &path, "run-c", "edit_file", "b.txt"))
                    .await
            })
        };
        observer.wait_for("waiting:workspace:run-c:edit_file").await;
        lease_a.release().await;
        let mut lease_b = tokio::time::timeout(std::time::Duration::from_secs(2), b)
            .await
            .expect("B acquires after A releases")
            .expect("join")
            .expect("workspace lease");
        // C must still be waiting while B holds the exclusive lease.
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(80), &mut c)
                .await
                .is_err(),
            "C may not overtake the exclusive lease"
        );
        lease_b.release().await;
        let mut lease_c = tokio::time::timeout(std::time::Duration::from_secs(2), c)
            .await
            .expect("C acquires after B releases")
            .expect("join")
            .expect("file lease");
        lease_c.release().await;
    }

    #[tokio::test]
    async fn wait_cap_produces_busy_with_holders() {
        let coordinator = coordinator();
        let root = temp_dir("busy");
        let file = format!("{root}/a.txt");
        let mut held = coordinator
            .acquire_file(file_input(
                &root,
                &file,
                "run-holder",
                "run_command",
                "cargo test",
            ))
            .await
            .unwrap();
        let held_result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            coordinator.acquire_file(AcquireFileInput {
                root: root.clone(),
                path: file.clone(),
                holder: holder("run-waiter", "edit_file", "a.txt"),
                cancellation: None,
                max_wait_ms: Some(50),
                execution_id: None,
            }),
        )
        .await
        .expect("busy fires promptly");
        let error = match held_result {
            Ok(_) => panic!("must time out"),
            Err(error) => error,
        };
        assert_eq!(error.code, "workspace_busy");
        assert!(
            error
                .message
                .contains("another session is running run_command: cargo test"),
            "{error}"
        );
        assert!(
            error.message.contains("(waited 50ms). Try again later."),
            "{error}"
        );
        held.release().await;
    }

    #[tokio::test]
    async fn cancelled_waiter_frees_the_queue() {
        let observer = RecordingObserver::new();
        let coordinator = coordinator_with(observer.clone());
        let root = temp_dir("cancel");
        let file = format!("{root}/a.txt");
        let mut held = coordinator
            .acquire_file(file_input(&root, &file, "run-holder", "edit_file", "a.txt"))
            .await
            .unwrap();
        let token = CancellationToken::new();
        let waiter = {
            let coordinator = Arc::clone(&coordinator);
            let root = root.clone();
            let file = file.clone();
            let token = token.clone();
            tokio::spawn(async move {
                coordinator
                    .acquire_file(AcquireFileInput {
                        root,
                        path: file,
                        holder: holder("run-cancelled", "edit_file", "a.txt"),
                        cancellation: Some(token),
                        max_wait_ms: None,
                        execution_id: None,
                    })
                    .await
            })
        };
        observer
            .wait_for("waiting:file:run-cancelled:edit_file")
            .await;
        token.cancel();
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(2), waiter)
            .await
            .expect("cancelled promptly")
            .expect("join");
        let error = match outcome {
            Ok(_) => panic!("must abort"),
            Err(error) => error,
        };
        assert_eq!(error.code, "aborted");
        held.release().await;
        let mut fresh = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            coordinator.acquire_file(file_input(&root, &file, "run-next", "edit_file", "a.txt")),
        )
        .await
        .expect("queue not stuck")
        .expect("acquire");
        fresh.release().await;
    }

    #[tokio::test]
    async fn workspace_lease_excludes_file_leases() {
        let coordinator = coordinator();
        let root = temp_dir("workspace-exclusive");
        let file = format!("{root}/a.txt");
        let mut exclusive = coordinator
            .acquire_workspace(AcquireWorkspaceInput {
                root: root.clone(),
                holder: holder("run-cmd", "run_command", "bun test"),
                cancellation: None,
                max_wait_ms: None,
                execution_id: None,
            })
            .await
            .unwrap();
        let blocked = tokio::time::timeout(
            std::time::Duration::from_millis(150),
            coordinator.acquire_file(file_input(&root, &file, "run-edit", "edit_file", "a.txt")),
        )
        .await;
        assert!(
            blocked.is_err(),
            "file lease must wait behind the workspace lease"
        );
        exclusive.release().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_dropped_holder_future_still_releases_the_workspace() {
        // TS releases in `finally`; the Rust core harness DROPS the tool
        // future on timeout/abort. The lease must still be reclaimed.
        let coordinator = coordinator();
        let root = temp_dir("drop-holder");
        let (started_tx, started_rx) = oneshot::channel();
        let inner = Arc::clone(&coordinator);
        let inner_root = root.clone();
        let holder_task = tokio::spawn(async move {
            let mut lease = inner
                .acquire_workspace(AcquireWorkspaceInput {
                    root: inner_root,
                    holder: holder("drop-run", "run_command", "held"),
                    cancellation: None,
                    max_wait_ms: None,
                    execution_id: None,
                })
                .await
                .expect("exclusive acquire");
            let _ = started_tx.send(());
            std::future::pending::<()>().await;
            lease.release().await;
        });
        started_rx.await.expect("held");
        holder_task.abort();
        let _ = holder_task.await;
        // Let the Drop-deferred release run, then re-acquire.
        tokio::time::sleep(Duration::from_millis(150)).await;
        let mut reclaim = tokio::time::timeout(
            Duration::from_secs(5),
            coordinator.acquire_workspace(AcquireWorkspaceInput {
                root,
                holder: holder("reclaim-run", "run_command", "reclaim"),
                cancellation: None,
                max_wait_ms: None,
                execution_id: None,
            }),
        )
        .await
        .expect("workspace must not stay wedged")
        .expect("reclaim grants");
        reclaim.release().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_abandoned_queued_waiter_does_not_wedge_the_gate() {
        // The queued waiter's future is dropped before `pump` can grant it;
        // the entry must vanish, or a later exclusive request waits forever
        // behind a holder that no longer exists.
        let coordinator = coordinator();
        let root = temp_dir("drop-queue");
        let mut held = coordinator
            .acquire_workspace(AcquireWorkspaceInput {
                root: root.clone(),
                holder: holder("queue-a", "run_command", "a"),
                cancellation: None,
                max_wait_ms: None,
                execution_id: None,
            })
            .await
            .expect("a acquires");
        let inner = Arc::clone(&coordinator);
        let inner_root = root.clone();
        let queued = tokio::spawn(async move {
            let mut lease = inner
                .acquire_workspace(AcquireWorkspaceInput {
                    root: inner_root,
                    holder: holder("queue-b", "run_command", "b"),
                    cancellation: None,
                    max_wait_ms: None,
                    execution_id: None,
                })
                .await
                .expect("b acquires");
            lease.release().await;
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        queued.abort();
        let _ = queued.await;
        held.release().await;
        let mut reclaim = tokio::time::timeout(
            Duration::from_secs(5),
            coordinator.acquire_workspace(AcquireWorkspaceInput {
                root,
                holder: holder("queue-c", "run_command", "c"),
                cancellation: None,
                max_wait_ms: None,
                execution_id: None,
            }),
        )
        .await
        .expect("gate must not be wedged by an abandoned waiter")
        .expect("c acquires");
        reclaim.release().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_abandoned_file_acquire_releases_its_shared_part() {
        // The future is dropped AFTER the shared workspace part was granted
        // and while it still waits for the file lock: the shared release
        // closure exists but never reaches a lease — `PendingRelease::drop`
        // must reclaim it, or the leaked shared holder wedges every later
        // workspace-exclusive lease.
        let coordinator = coordinator();
        let root = temp_dir("drop-file");
        let file = format!("{root}/f.txt");
        let mut holder_c = coordinator
            .acquire_file(file_input(&root, &file, "c", "edit_file", "f.txt"))
            .await
            .expect("c acquires");
        let inner = Arc::clone(&coordinator);
        let inner_root = root.clone();
        let inner_file = file.clone();
        let waiter = tokio::spawn(async move {
            let mut lease = inner
                .acquire_file(file_input(
                    &inner_root,
                    &inner_file,
                    "d",
                    "edit_file",
                    "f.txt",
                ))
                .await
                .expect("d acquires");
            lease.release().await;
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        waiter.abort();
        let _ = waiter.await;
        holder_c.release().await;
        let mut reclaim = tokio::time::timeout(
            Duration::from_secs(5),
            coordinator.acquire_workspace(AcquireWorkspaceInput {
                root,
                holder: holder("e", "run_command", "reclaim"),
                cancellation: None,
                max_wait_ms: None,
                execution_id: None,
            }),
        )
        .await
        .expect("workspace must not stay wedged by an abandoned file acquire")
        .expect("workspace exclusive grants");
        reclaim.release().await;
    }

    /// Defect B2, workspace lease. A holds the exclusive gate; B is confirmed
    /// queued by the observer (its `await_clear` has therefore already run);
    /// the quarantine is published while A still holds; A releases. Pre-fix, B
    /// was granted the lease because nothing re-checked the evidence after the
    /// grant — the queued writer slipped past a live quarantine.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_queued_workspace_writer_is_refused_after_a_quarantine_lands() {
        // A fresh coordinator per case: the process-global shared instance
        // would leak a quarantine into every other test.
        let observer = RecordingObserver::new();
        let coordinator = coordinator_with(observer.clone());
        let root = temp_dir("queued-ws-quarantine");
        let mut held = coordinator
            .acquire_workspace(ws_input(&root, "run-a", "run_command", "make install"))
            .await
            .expect("A takes the workspace");
        let queued = {
            let coordinator = Arc::clone(&coordinator);
            let root = root.clone();
            tokio::spawn(async move {
                coordinator
                    .acquire_workspace(ws_input(&root, "run-b", "run_command", "make test"))
                    .await
            })
        };
        // Deterministic ordering, no sleep: B being queued is proven before
        // the record exists.
        observer
            .wait_for("waiting:workspace:run-b:run_command")
            .await;
        let (pid, child) = ChildGuard::spawn("sleep", &["60"]);
        // The documented invariant: the quarantining caller publishes while it
        // still holds its lease, so the record is visible before the gate can
        // move to B.
        coordinator
            .quarantine_workspace(
                &root,
                &QuarantineEvidence {
                    pids: vec![pid],
                    holder: holder("run-a", "run_command", "make install"),
                    reason: None,
                },
            )
            .await
            .expect("blocking evidence publishes");
        held.release().await;
        let error = tokio::time::timeout(Duration::from_secs(10), queued)
            .await
            .expect("B resolves once A releases")
            .expect("join")
            .expect_err("a live quarantine must not hand B an executable lease");
        assert_eq!(error.code, "workspace_quarantined", "{error:?}");
        assert!(error.message.contains("make install"), "{error}");
        assert!(error.message.contains(&pid.to_string()), "{error}");
        // The refusal must give back the gate it had just taken.
        child.reap();
        let mut reclaim = tokio::time::timeout(
            Duration::from_secs(10),
            coordinator.acquire_workspace(ws_input(&root, "run-c", "run_command", "make test")),
        )
        .await
        .expect("the workspace is reclaimable once the survivor exited")
        .expect("C acquires");
        reclaim.release().await;
    }

    /// The same race on the edit path: a queued FILE lease must not come out
    /// of the gate holding a writable lease over a quarantined workspace.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_queued_file_writer_is_refused_after_a_quarantine_lands() {
        let observer = RecordingObserver::new();
        let coordinator = coordinator_with(observer.clone());
        let root = temp_dir("queued-file-quarantine");
        let file = format!("{root}/a.txt");
        let mut held = coordinator
            .acquire_file(file_input(&root, &file, "run-a", "edit_file", "edit a.txt"))
            .await
            .expect("A holds the file lease");
        let queued = {
            let coordinator = Arc::clone(&coordinator);
            let root = root.clone();
            let file = file.clone();
            tokio::spawn(async move {
                coordinator
                    .acquire_file(file_input(&root, &file, "run-b", "write_file", "a.txt"))
                    .await
            })
        };
        observer.wait_for("waiting:file:run-b:write_file").await;
        let (pid, child) = ChildGuard::spawn("sleep", &["60"]);
        coordinator
            .quarantine_workspace(
                &root,
                &QuarantineEvidence {
                    pids: vec![pid],
                    holder: holder("run-a", "edit_file", "edit a.txt"),
                    reason: None,
                },
            )
            .await
            .expect("blocking evidence publishes");
        held.release().await;
        let error = tokio::time::timeout(Duration::from_secs(10), queued)
            .await
            .expect("B resolves once A releases")
            .expect("join")
            .expect_err("a live quarantine must not hand B an executable file lease");
        assert_eq!(error.code, "workspace_quarantined", "{error:?}");
        assert!(error.message.contains(&pid.to_string()), "{error}");
        // Both halves (exclusive file gate + shared workspace gate) came back.
        child.reap();
        let mut reclaim = tokio::time::timeout(
            Duration::from_secs(10),
            coordinator.acquire_workspace(ws_input(&root, "run-c", "run_command", "make test")),
        )
        .await
        .expect("the workspace is reclaimable once the survivor exited")
        .expect("C acquires the exclusive lease the file lease had blocked");
        reclaim.release().await;
    }

    // ---- K1: grants are tracked by entry id, not holder equality ----

    #[tokio::test]
    async fn an_abandoned_queued_waiter_with_an_equal_holder_keeps_the_first_grant() {
        let map = Arc::new(GateMap::new());
        // Identical holders: no tool_call_id, same run — `==` cannot tell them apart.
        let same = holder("same-run", "command", "x");
        let first = map.try_acquire("k", Mode::Exclusive, &same);
        let first_id = map
            .wait("k", first, &None, &|_| {})
            .await
            .expect("first granted");
        let second = map.try_acquire("k", Mode::Exclusive, &same);
        assert!(matches!(second, AcquireState::Queued { .. }));
        {
            let waiting = map.wait("k", second, &None, &|_| {});
            tokio::pin!(waiting);
            assert!(futures::poll!(&mut waiting).is_pending());
        } // second's future dropped while queued
        // The first grant is still held: a third request must queue.
        let third = map.try_acquire("k", Mode::Exclusive, &same);
        let AcquireState::Queued { receiver, .. } = third else {
            panic!("the first holder's exclusive grant was released by a dropped waiter");
        };
        map.release("k", first_id);
        receiver
            .await
            .expect("third is granted once the first releases");
    }

    #[tokio::test]
    async fn a_granted_but_unaccepted_waiter_drop_removes_only_its_own_grant() {
        let map = Arc::new(GateMap::new());
        let same = holder("same-run", "command", "x");
        let first = map.try_acquire("k", Mode::Exclusive, &same);
        let first_id = map.wait("k", first, &None, &|_| {}).await.expect("first");
        let second = map.try_acquire("k", Mode::Exclusive, &same);
        let mut waiting = Box::pin(map.wait("k", second, &None, &|_| {}));
        assert!(futures::poll!(&mut waiting).is_pending());
        // `pump` grants the second entry; its future never runs again.
        map.release("k", first_id);
        drop(waiting);
        // Nothing is left held: the next request is granted immediately.
        assert!(matches!(
            map.try_acquire("k", Mode::Exclusive, &same),
            AcquireState::Granted(_)
        ));
    }

    // ---- K3: a half-released lease still defers the remaining closures ----

    #[tokio::test]
    async fn dropping_a_lease_mid_release_defers_the_remaining_releases() {
        let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
        let (second_tx, second_rx) = tokio::sync::oneshot::channel::<()>();
        let first: ReleaseFn = Box::new(move || {
            Box::pin(async move {
                let _ = started_tx.send(());
                futures::future::pending::<()>().await;
            })
        });
        let second: ReleaseFn = Box::new(move || {
            Box::pin(async move {
                let _ = second_tx.send(());
            })
        });
        let mut lease = WorkspaceLease::new(vec![first, second]);
        {
            let releasing = lease.release();
            tokio::pin!(releasing);
            assert!(futures::poll!(&mut releasing).is_pending());
            started_rx.await.expect("first release started");
        } // dropped mid-release
        drop(lease);
        tokio::time::timeout(Duration::from_secs(5), second_rx)
            .await
            .expect("the remaining release runs via Drop")
            .expect("second release ran");
    }

    // ---- K6: the wait cap spawns no forwarder / timer task ----

    #[tokio::test]
    async fn the_wait_cap_spawns_no_task_and_follows_the_caller_token() {
        let parent = CancellationToken::new();
        let tracker = WaitTracker::default();
        let observed_tasks = Arc::new(AtomicU64::new(u64::MAX));
        let seen = Arc::clone(&observed_tasks);
        let waiting = with_wait_cap(Some(parent.clone()), Some(60_000), &tracker, move |token| {
            Box::pin(async move {
                let alive = tokio::runtime::Handle::current()
                    .metrics()
                    .num_alive_tasks();
                seen.store(alive as u64, Ordering::SeqCst);
                token.expect("capped token").cancelled().await;
                Err::<(), _>(abort_error())
            })
        });
        tokio::pin!(waiting);
        assert!(futures::poll!(&mut waiting).is_pending());
        parent.cancel();
        let error = waiting
            .await
            .expect_err("the caller's cancel reaches the capped token");
        assert_eq!(error.code, "aborted", "not reported as busy: {error:?}");
        assert_eq!(observed_tasks.load(Ordering::SeqCst), 0, "no spawned task");
    }

    // ---- K7: observers run outside the gate lock ----

    struct PanickingObserver;

    impl CoordinatorObserver for PanickingObserver {
        fn on_waiting(&self, _wait: &CoordinationWait) {
            panic!("observer bug");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_panicking_observer_neither_poisons_nor_wedges_the_gate() {
        let coordinator = coordinator_with(Arc::new(PanickingObserver));
        let root = temp_dir("panicking-observer");
        let mut held = coordinator
            .acquire_workspace(ws_input(&root, "obs-a", "command", "a"))
            .await
            .expect("a acquires");
        let blocked = {
            let (coordinator, root) = (Arc::clone(&coordinator), root.clone());
            tokio::spawn(async move {
                coordinator
                    .acquire_workspace(ws_input(&root, "obs-b", "command", "b"))
                    .await
            })
        };
        let joined = blocked.await;
        assert!(
            joined.is_err_and(|error| error.is_panic()),
            "observer panicked in b"
        );
        held.release().await;
        let mut later = tokio::time::timeout(
            Duration::from_secs(5),
            coordinator.acquire_workspace(ws_input(&root, "obs-c", "command", "c")),
        )
        .await
        .expect("the gate is not wedged or poisoned")
        .expect("c acquires");
        later.release().await;
    }

    // ---- K4: retire is rename + verify, never check-then-unlink ----

    fn quarantine_at(stamp: u64, run: &str) -> WorkspaceQuarantine {
        WorkspaceQuarantine {
            root: "/r".into(),
            pids: vec![4242],
            holder: holder(run, "command", "x"),
            quarantined_at_ms: stamp,
            reason: None,
            identity: None,
        }
    }

    fn write_quarantine(path: &std::path::Path, quarantine: &WorkspaceQuarantine) {
        std::fs::write(path, quarantine_json(quarantine).to_string()).expect("write record");
    }

    #[tokio::test]
    async fn retire_unlinks_the_record_that_was_scanned() {
        let dir = std::path::PathBuf::from(temp_dir("retire-same"));
        let path = dir.join(QUARANTINE_FILE_NAME);
        let scanned = quarantine_at(100, "run-a");
        write_quarantine(&path, &scanned);
        assert!(
            retire_quarantine_record(&path, "/r", &scanned)
                .await
                .unwrap()
        );
        assert!(!path.exists(), "the scanned record is retired");
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            0,
            "no tombstone left"
        );
    }

    #[tokio::test]
    async fn retire_racing_a_republish_keeps_the_new_record() {
        let dir = std::path::PathBuf::from(temp_dir("retire-race"));
        let path = dir.join(QUARANTINE_FILE_NAME);
        let scanned = quarantine_at(100, "run-a");
        // Between the scan and the retire a NEW unresolved cleanup replaced it.
        write_quarantine(&path, &quarantine_at(200, "run-b"));
        let retired = retire_quarantine_record(&path, "/r", &scanned)
            .await
            .unwrap();
        assert!(
            !retired,
            "a different record must report 'still quarantined'"
        );
        let kept: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("record restored"))
                .unwrap();
        assert_eq!(kept["runId"], "run-b", "the newer record survives");
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            1,
            "no tombstone left"
        );
    }

    #[tokio::test]
    async fn a_displaced_record_whose_name_was_retaken_stays_in_its_tombstone() {
        let dir = std::path::PathBuf::from(temp_dir("retire-retaken"));
        let path = dir.join(QUARANTINE_FILE_NAME);
        let tomb = dir.join("quarantine.json.retiring.1.2");
        write_quarantine(&tomb, &quarantine_at(100, "run-old"));
        write_quarantine(&path, &quarantine_at(200, "run-new"));
        restore_tombstone(&tomb, &path);
        assert!(tomb.exists(), "the displaced record is kept, not destroyed");
        let kept = std::fs::read_to_string(&path).unwrap();
        assert!(kept.contains("run-new"), "the newer record wins the name");
    }

    // ---- K5: private journal, digest-only labels, strict pid parsing ----

    #[cfg(unix)]
    #[test]
    fn the_journal_directory_check_refuses_loose_symlinked_and_accepts_private() {
        use std::os::unix::fs::PermissionsExt;
        let base = std::path::PathBuf::from(temp_dir("private-check"));
        let dir = base.join("d");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(check_private_dir(&dir).unwrap());
        for loose in [0o755, 0o750, 0o770, 0o707] {
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(loose)).unwrap();
            assert!(
                check_private_dir(&dir).is_err(),
                "mode {loose:o} must be refused"
            );
        }
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let link = base.join("link");
        std::os::unix::fs::symlink(&dir, &link).unwrap();
        assert!(check_private_dir(&link).is_err(), "symlinks are refused");
        assert!(!check_private_dir(&base.join("missing")).unwrap());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn journal_files_are_owner_only_and_carry_no_clear_label() {
        use std::os::unix::fs::PermissionsExt;
        let root_key = format!("/k5-root-{}", uuid::Uuid::new_v4());
        let secret_label = "super secret command label";
        let intent = ExecutionIntent {
            root: root_key.clone(),
            identity: CommandCleanupIdentity {
                execution_id: format!("{}:{}", std::process::id(), uuid::Uuid::new_v4()),
                owner_pid: i64::from(std::process::id()),
                pgid: Some(4242),
                spawned_at_ms: 1,
            },
            holder: holder("k5", "command", secret_label),
            spawned: true,
        };
        write_intent_file(&root_key, &intent).await.expect("intent");
        let quarantine = WorkspaceQuarantine {
            root: root_key.clone(),
            pids: vec![4242],
            holder: holder("k5", "command", secret_label),
            quarantined_at_ms: 5,
            reason: None,
            identity: None,
        };
        write_quarantine_file(&root_key, &quarantine)
            .await
            .expect("quarantine");
        let leaf = intent_dir_for(&root_key);
        let mode =
            |path: &std::path::Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&intent_journal_dir()), 0o700);
        assert_eq!(mode(&leaf), 0o700);
        let mut files = 0;
        for entry in std::fs::read_dir(&leaf).unwrap() {
            let path = entry.unwrap().path();
            assert_eq!(mode(&path), 0o600, "{}", path.display());
            let text = std::fs::read_to_string(&path).unwrap();
            assert!(!text.contains(secret_label), "clear label on disk: {text}");
            assert!(text.contains("labelSha256"), "{text}");
            files += 1;
        }
        assert_eq!(files, 2);
        let back = read_quarantine_file(&root_key)
            .await
            .unwrap()
            .expect("readable");
        assert_eq!(back.pids, vec![4242]);
        let _ = std::fs::remove_dir_all(&leaf);
    }

    #[test]
    fn records_with_bad_pids_are_rejected_and_legacy_labels_are_ignored() {
        let base = serde_json::json!({
            "executionId": "1:abc", "ownerPid": 7, "pgid": 9, "runId": "r",
            "tool": "command", "label": "clear legacy label",
        });
        let legacy = intent_from_json(&base).expect("legacy record still parses");
        assert_eq!(
            legacy.holder.label, "unknown",
            "legacy clear label is ignored"
        );
        for bad in [
            serde_json::json!(0),
            serde_json::json!(-5),
            serde_json::json!(i64::from(i32::MAX) + 1),
            serde_json::json!("12"),
        ] {
            let mut value = base.clone();
            value["pgid"] = bad.clone();
            assert!(intent_from_json(&value).is_none(), "pgid {bad}");
            let mut value = base.clone();
            value["ownerPid"] = bad.clone();
            assert!(intent_from_json(&value).is_none(), "ownerPid {bad}");
            let record = serde_json::json!({"pids": [bad.clone()]});
            assert!(
                quarantine_from_json("/r", &record).is_none(),
                "pids [{bad}]"
            );
        }
    }

    // ---- review fixes: legacy journal, in-memory mirror on mismatch ----

    #[cfg(unix)]
    #[tokio::test]
    async fn pending_records_in_the_legacy_shared_journal_are_still_honored() {
        use std::os::unix::fs::MetadataExt;
        let legacy_base = std::env::temp_dir().join("agentkit-exec-intents");
        if let Ok(meta) = std::fs::symlink_metadata(&legacy_base)
            && (meta.uid() != current_uid() || !meta.file_type().is_dir())
        {
            return; // a foreign legacy dir is (correctly) ignored; nothing to assert
        }
        let root_key = format!("/legacy-root-{}", uuid::Uuid::new_v4());
        let leaf = legacy_base.join(crate::fs_util::sha256(root_key.as_bytes()));
        std::fs::create_dir_all(&leaf).expect("legacy leaf");
        std::fs::write(
            leaf.join(QUARANTINE_FILE_NAME),
            serde_json::json!({
                "pids": [std::process::id()], "quarantinedAtMs": 9, "runId": "old-run",
                "tool": "command", "label": "clear old label",
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            leaf.join("11-abc.json"),
            serde_json::json!({
                "executionId": "11:abc", "ownerPid": 11, "runId": "old-run",
                "tool": "command", "label": "clear old label", "spawned": false,
            })
            .to_string(),
        )
        .unwrap();
        let quarantine = read_quarantine_file(&root_key)
            .await
            .unwrap()
            .expect("legacy quarantine honored");
        assert_eq!(quarantine.holder.run_id, "old-run");
        assert_eq!(quarantine.pids, vec![std::process::id() as i32]);
        let intents = read_intent_files(&root_key).await.unwrap();
        assert_eq!(intents.len(), 1, "legacy intent honored");
        assert_eq!(intents[0].identity.execution_id, "11:abc");
        // Recovery removes the legacy copies too.
        remove_quarantine_file(&root_key).await.unwrap();
        remove_intent_file(&root_key, "11:abc").await.unwrap();
        assert!(read_quarantine_file(&root_key).await.unwrap().is_none());
        assert!(read_intent_files(&root_key).await.unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&leaf);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_retire_mismatch_drops_the_stale_memory_mirror_and_reports_the_durable_record() {
        let coordinator = InProcessCoordinator::new(None);
        let root_key = format!("/mirror-root-{}", uuid::Uuid::new_v4());
        let mut child = tokio::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let dead = child.id().expect("pid") as i32;
        let _ = child.kill().await;
        let _ = child.wait().await;
        // Memory mirrors a record whose process is gone...
        let judged = WorkspaceQuarantine {
            root: root_key.clone(),
            pids: vec![dead],
            holder: holder("old-run", "command", "x"),
            quarantined_at_ms: 1,
            reason: None,
            identity: None,
        };
        lock(&coordinator.quarantines).insert(root_key.clone(), judged);
        // ...but a different, live record was durably published since.
        let durable = WorkspaceQuarantine {
            root: root_key.clone(),
            pids: vec![std::process::id() as i32],
            holder: holder("new-run", "command", "y"),
            quarantined_at_ms: 2,
            reason: None,
            identity: None,
        };
        write_quarantine_file(&root_key, &durable).await.unwrap();
        let reported = coordinator
            .live_quarantine(&root_key)
            .await
            .unwrap()
            .expect("the durable record governs");
        assert_eq!(reported.holder.run_id, "new-run");
        assert!(
            !lock(&coordinator.quarantines).contains_key(&root_key),
            "stale mirror dropped"
        );
        let _ = std::fs::remove_dir_all(intent_dir_for(&root_key));
    }

    #[tokio::test]
    async fn the_quarantine_reason_round_trips_and_names_itself_in_the_error() {
        let root_key = format!("/reason-root-{}", uuid::Uuid::new_v4());
        let mut quarantine = quarantine_at(7, "run-r");
        quarantine.root = root_key.clone();
        quarantine.reason = Some("group_stuck".to_owned());
        write_quarantine_file(&root_key, &quarantine).await.unwrap();
        let back = read_quarantine_file(&root_key).await.unwrap().unwrap();
        assert_eq!(back.reason.as_deref(), Some("group_stuck"));
        assert!(quarantine_error(&back).message.contains("group_stuck"));
        // A record without the field (older build) reads back as None.
        let mut value = quarantine_json(&quarantine);
        value.as_object_mut().unwrap().remove("reason");
        assert!(
            quarantine_from_json(&root_key, &value)
                .unwrap()
                .reason
                .is_none()
        );
        let _ = std::fs::remove_dir_all(intent_dir_for(&root_key));
    }

    /// A pid that can never be alive (far above any pid_max) and can
    /// never be recycled by a concurrent test's spawn.
    fn certainly_dead() -> i32 {
        2_000_000_001
    }

    #[tokio::test]
    async fn retiring_tombstone_litter_is_swept_and_live_evidence_survives() {
        // AGENTKIT-74 cleanup sweep: a retirement tombstone naming only
        // dead processes is litter; live-pid evidence and an explicit-
        // recovery (empty-pid) record stay; a tombstone another task
        // holds the flock on is never raced.
        let dir = std::path::PathBuf::from(temp_dir("retiring-sweep"));
        let dead = certainly_dead();
        let write = |name: &str, body: &str| {
            let path = dir.join(name);
            std::fs::write(&path, body).expect("tomb");
            path
        };
        let gone = write(
            "quarantine.json.retiring.dead",
            &serde_json::json!({"pids": [dead], "quarantinedAtMs": 9,
                                "runId": "r", "tool": "command", "label": "x"})
            .to_string(),
        );
        let litter = write("quarantine.json.retiring.broken", "not json");
        let live = write(
            "quarantine.json.retiring.alive",
            &serde_json::json!({"pids": [std::process::id() as i32], "quarantinedAtMs": 9,
                                "runId": "r", "tool": "command", "label": "x"})
            .to_string(),
        );
        let blocked = write(
            "quarantine.json.retiring.empty",
            &serde_json::json!({"pids": [], "quarantinedAtMs": 9,
                                "runId": "r", "tool": "command", "label": "x"})
            .to_string(),
        );
        let unreadable = write("quarantine.json.retiring.unreadable", "[]");
        let held = write(
            "quarantine.json.retiring.held",
            &serde_json::json!({"pids": [dead], "quarantinedAtMs": 9,
                                "runId": "r", "tool": "command", "label": "x"})
            .to_string(),
        );
        let _hold = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&held)
            .expect("open held");
        assert!(_hold.try_lock().is_ok(), "flock the candidate");
        // Grace window first: nothing is eligible before it lapses.
        assert!(
            sweep_retiring_tombstones_sync(dir.clone(), i64::MAX / 2).is_empty(),
            "grace"
        );
        let swept = sweep_retiring_tombstones_sync(dir.clone(), 0);
        assert!(
            swept.contains(&gone) && swept.contains(&litter),
            "dead-only and unparseable tombstones swept: {swept:?}"
        );
        for keep in [&live, &blocked, &held, &unreadable] {
            assert!(keep.exists(), "{keep:?} must survive the sweep");
        }
        drop(_hold);
        // After the flock releases, the dead-only tombstone goes too.
        // A concurrent spawn in THIS process can transiently inherit the
        // flock we just dropped: between fork and exec the child holds a
        // second reference to the descriptor (exec closes it via
        // O_CLOEXEC), so one sweep call can legitimately hit EWOULDBLOCK
        // and defer. Production treats the sweep as best-effort hygiene
        // repeated on every coordinator open; the test retries the same
        // way instead of asserting one call succeeds under any load.
        let mut swept_held = false;
        for _ in 0..200 {
            if sweep_retiring_tombstones_sync(dir.clone(), 0).contains(&held) {
                swept_held = true;
                break;
            }
            if !held.exists() {
                swept_held = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(
            swept_held,
            "held tombstone must be swept once no flock holder survives"
        );
        assert!(!held.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn abandoned_legacy_journal_leaves_are_swept_live_leaves_are_not() {
        // AGENTKIT-74 cleanup sweep: the world-shared legacy journal
        // directory keeps only leaves whose records name exclusively dead
        // processes; any live pid, empty quarantine, or unparseable
        // record keeps its whole leaf. A foreign (other-uid) base is
        // never entered.
        use std::os::unix::fs::MetadataExt;
        let base = std::env::temp_dir().join("agentkit-exec-intents");
        if let Ok(meta) = std::fs::symlink_metadata(&base)
            && (meta.uid() != current_uid() || !meta.file_type().is_dir())
        {
            return; // a foreign legacy dir is (correctly) never touched
        }
        std::fs::create_dir_all(&base).expect("legacy base");
        let dead = certainly_dead();
        let abandoned = base.join("abandoned-leaf");
        std::fs::create_dir_all(&abandoned).expect("leaf");
        std::fs::write(
            abandoned.join(QUARANTINE_FILE_NAME),
            serde_json::json!({"pids": [dead], "quarantinedAtMs": 9,
                               "runId": "old", "tool": "command", "label": "clear"})
            .to_string(),
        )
        .expect("write");
        std::fs::write(
            abandoned.join("11-abc.json"),
            serde_json::json!({"executionId": "11:abc", "ownerPid": dead,
                               "runId": "old", "tool": "command", "label": "clear"})
            .to_string(),
        )
        .expect("write");
        let live_root = base.join("live-leaf");
        std::fs::create_dir_all(&live_root).expect("leaf 2");
        std::fs::write(
            live_root.join(QUARANTINE_FILE_NAME),
            serde_json::json!({"pids": [std::process::id() as i32], "quarantinedAtMs": 9,
                               "runId": "run", "tool": "command", "label": "clear"})
            .to_string(),
        )
        .expect("write");
        sweep_legacy_journal(0);
        assert!(
            !abandoned.exists(),
            "every pid dead: the abandoned legacy leaf is swept"
        );
        assert!(
            live_root.join(QUARANTINE_FILE_NAME).exists(),
            "a live pid keeps the whole leaf"
        );
        // Fail-closed shapes: empty-pid quarantine and an unparseable
        // intent each pin their leaf.
        let blocked = base.join("blocked-leaf");
        std::fs::create_dir_all(&blocked).expect("leaf 3");
        std::fs::write(
            blocked.join(QUARANTINE_FILE_NAME),
            serde_json::json!({"pids": [], "quarantinedAtMs": 9,
                               "runId": "b", "tool": "command", "label": "clear"})
            .to_string(),
        )
        .expect("write");
        sweep_legacy_journal(0);
        assert!(blocked.exists(), "empty pids block until explicit recovery");
        let corrupt = base.join("corrupt-leaf");
        std::fs::create_dir_all(&corrupt).expect("leaf 4");
        std::fs::write(corrupt.join("intent.json"), "not json").expect("write");
        sweep_legacy_journal(0);
        assert!(corrupt.exists(), "unparseable records reconcile as unknown");
        for leaf in [live_root, blocked, corrupt] {
            let _ = std::fs::remove_dir_all(leaf);
        }
        let _ = std::fs::remove_dir(&base);
    }
}
