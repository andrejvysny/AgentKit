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
use std::sync::{Arc, LazyLock, Mutex};

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
        if let Some(releases) = self.releases.take() {
            for release in releases {
                release().await;
            }
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
}

#[derive(Clone)]
pub struct AcquireWorkspaceInput {
    pub root: String,
    pub holder: LeaseHolder,
    pub cancellation: Option<CancellationToken>,
    pub max_wait_ms: Option<u64>,
}

/// Evidence that cleanup was not positively established: the command's
/// process tree still had live members when the lease was released.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuarantineEvidence {
    pub pids: Vec<i32>,
    pub holder: LeaseHolder,
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
}

/// Cleanup is positively established when the record lists processes and
/// none of them is alive. An empty pid list is an *unknown* survivor set
/// (e.g. an unreadable durable record) and stays blocked until explicit
/// recovery; a pid of `-1` is never probed.
pub(crate) fn all_quarantined_exited(pids: &[i32]) -> bool {
    !pids.is_empty()
        && pids
            .iter()
            .all(|pid| !crate::process::owner_alive(*pid as i64))
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
    CodingError::new(
        "workspace_quarantined",
        format!(
            "workspace cleanup is unresolved: {who} from {} '{}' (run {}) may still be writing; the workspace stays blocked until the processes exit or recovery is recorded.",
            quarantine.holder.tool, quarantine.holder.label, quarantine.holder.run_id
        ),
    )
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
    /// established or recovery is recorded.
    async fn quarantine_workspace(&self, root: &str, evidence: &QuarantineEvidence);
    /// Live quarantine for the root, if any. Records whose processes have
    /// all exited are cleared here (positive cleanup evidence).
    async fn workspace_quarantine(
        &self,
        root: &str,
    ) -> Result<Option<WorkspaceQuarantine>, CodingError>;
    /// Explicit operator recovery from an unresolved state. Returns whether
    /// a record was removed.
    async fn clear_workspace_quarantine(&self, root: &str) -> Result<bool, CodingError>;
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
            let mut state = self.state.lock().expect("coordination tracker lock");
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
            .expect("coordination tracker lock")
            .last
            .as_ref()
            .map(|wait| wait.holders.clone())
            .unwrap_or_default()
    }

    pub(crate) fn acquired(&self, observer: Option<&Arc<dyn CoordinatorObserver>>) {
        let snapshot = self.state.lock().expect("coordination tracker lock");
        if let (true, Some(wait)) = (snapshot.fired, snapshot.last.clone())
            && let Some(observer) = observer
        {
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
/// exclusive-only.
#[derive(Default)]
struct Gate {
    shared: Vec<LeaseHolder>,
    exclusive: Option<LeaseHolder>,
    queue: VecDeque<Entry>,
}

impl Gate {
    fn holders(&self) -> Vec<LeaseHolder> {
        match &self.exclusive {
            Some(holder) => vec![holder.clone()],
            None => self.shared.clone(),
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

    fn take(&mut self, mode: Mode, holder: &LeaseHolder) {
        match mode {
            Mode::Exclusive => self.exclusive = Some(holder.clone()),
            Mode::Shared => self.shared.push(holder.clone()),
        }
    }

    fn pump(&mut self) {
        while let Some(front) = self.queue.front() {
            if !self.can_grant(front.mode) {
                return;
            }
            let mut entry = self.queue.pop_front().expect("front checked");
            self.take(entry.mode, &entry.holder);
            if let Some(grant) = entry.grant.take() {
                let _ = grant.send(());
            }
        }
    }
}

enum AcquireState {
    Granted,
    Queued {
        id: EntryId,
        receiver: oneshot::Receiver<()>,
        mode: Mode,
        holder: LeaseHolder,
    },
}

/// Reclaims a queued acquisition whose future never completes: removes the
/// queue entry, un-installs a grant that no one accepted, re-pumps the gate
/// and drops the key when idle. Disarmed once the waiter is granted (the
/// release closure owns the holder from then on).
struct AcquireGuard {
    map: Arc<GateMap>,
    key: String,
    id: EntryId,
    mode: Mode,
    holder: LeaseHolder,
    armed: bool,
}

impl Drop for AcquireGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut gates = self.map.gates.lock().expect("gate map lock");
        let Some(gate) = gates.get_mut(&self.key) else {
            return;
        };
        if let Some(at) = gate.queue.iter().position(|entry| entry.id == self.id) {
            gate.queue.remove(at);
        }
        match self.mode {
            Mode::Exclusive => {
                if gate.exclusive.as_ref() == Some(&self.holder) {
                    gate.exclusive = None;
                }
            }
            Mode::Shared => {
                if let Some(at) = gate.shared.iter().position(|h| h == &self.holder) {
                    gate.shared.remove(at);
                }
            }
        }
        gate.pump();
        if gate.idle() {
            gates.remove(&self.key);
        }
    }
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

    /// Try the fast path; on block, notify and enqueue a waiter.
    fn try_acquire(
        &self,
        key: &str,
        mode: Mode,
        holder: &LeaseHolder,
        on_block: &dyn Fn(&[LeaseHolder]),
    ) -> AcquireState {
        let mut gates = self.gates.lock().expect("gate map lock");
        let gate = gates.entry(key.to_owned()).or_default();
        if gate.queue.is_empty() && gate.can_grant(mode) {
            gate.take(mode, holder);
            return AcquireState::Granted;
        }
        on_block(&gate.holders());
        let (sender, receiver) = oneshot::channel();
        let id = EntryId(next_entry_id());
        gate.queue.push_back(Entry {
            id,
            mode,
            holder: holder.clone(),
            grant: Some(sender),
        });
        AcquireState::Queued {
            id,
            receiver,
            mode,
            holder: holder.clone(),
        }
    }

    /// Wait for the queued grant, honoring cancellation. An entry already
    /// granted by `pump` still runs its release afterwards. Every EARLY
    /// exit — including the future being dropped mid-await by the core
    /// harness — is reclaimed by [`AcquireGuard`].
    async fn wait(
        self: &Arc<Self>,
        key: &str,
        state: AcquireState,
        cancellation: &Option<CancellationToken>,
    ) -> Result<(), CodingError> {
        let AcquireState::Queued {
            id,
            mut receiver,
            mode,
            holder,
        } = state
        else {
            return Ok(());
        };
        let mut guard = AcquireGuard {
            map: Arc::clone(self),
            key: key.to_owned(),
            id,
            mode,
            holder,
            armed: true,
        };
        let outcome = if let Some(token) = cancellation {
            tokio::select! {
                biased;
                granted = &mut receiver => granted.map_err(|_| abort_error()),
                _ = token.cancelled() => {
                    // Prefer a grant that landed with the cancellation: a
                    // vanished queue entry means `pump` already installed
                    // this holder, and the lease must still be released.
                    let granted = {
                        let gates = self.gates.lock().expect("gate map lock");
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
            // The release closure now owns the holder's cleanup.
            guard.armed = false;
        }
        outcome
    }

    fn release(&self, key: &str, mode: Mode, holder: &LeaseHolder) {
        let mut gates = self.gates.lock().expect("gate map lock");
        let Some(gate) = gates.get_mut(key) else {
            return;
        };
        match mode {
            Mode::Exclusive => {
                if gate.exclusive.as_ref() == Some(holder) {
                    gate.exclusive = None;
                }
            }
            Mode::Shared => {
                if let Some(at) = gate.shared.iter().position(|h| h == holder) {
                    gate.shared.remove(at);
                }
            }
        }
        gate.pump();
        if gate.idle() {
            gates.remove(key);
        }
    }
}

fn sync_release(map: Arc<GateMap>, key: String, mode: Mode, holder: LeaseHolder) -> ReleaseFn {
    Box::new(move || {
        Box::pin(async move {
            map.release(&key, mode, &holder);
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
    let cap = CancellationToken::new();
    let busy = Arc::new(AtomicBool::new(false));
    if let Some(source) = cancellation.clone() {
        let forwarded = cap.clone();
        tokio::spawn(async move {
            source.cancelled().await;
            forwarded.cancel();
        });
    }
    let timer_busy = Arc::clone(&busy);
    let timer_cap = cap.clone();
    let timer = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(max_wait_ms)).await;
        timer_busy.store(true, Ordering::SeqCst);
        timer_cap.cancel();
    });
    let result = run(Some(cap)).await;
    timer.abort();
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
}

impl InProcessCoordinator {
    pub fn new(observer: Option<Arc<dyn CoordinatorObserver>>) -> Self {
        Self {
            workspaces: Arc::new(GateMap::new()),
            files: Arc::new(GateMap::new()),
            observer,
            quarantines: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn live_quarantine(&self, root: &str) -> Option<WorkspaceQuarantine> {
        let mut quarantines = self.quarantines.lock().expect("quarantine lock");
        let quarantine = quarantines.get(root)?;
        if all_quarantined_exited(&quarantine.pids) {
            // Positive cleanup evidence: every recorded process exited.
            quarantines.remove(root);
            return None;
        }
        Some(quarantine.clone())
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
            max_wait_ms.map(|ms| std::time::Instant::now() + std::time::Duration::from_millis(ms));
        loop {
            let Some(quarantine) = self.live_quarantine(root) else {
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
                    return Err(match self.live_quarantine(root) {
                        Some(quarantine) => quarantine_error(&quarantine),
                        None => abort_error(),
                    });
                }
                _ = tokio::time::sleep(std::time::Duration::from_millis(25)) => {}
            }
        }
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
        let state = {
            let tracker = tracker.clone();
            let observer_inner = observer.clone();
            let root_owned = root.to_owned();
            let path_owned = path.map(str::to_owned);
            let holder_owned = holder.clone();
            map.try_acquire(&key, mode, holder, &move |holders| {
                tracker.waiting(
                    CoordinationWait {
                        kind,
                        root: root_owned.clone(),
                        path: path_owned.clone(),
                        waiter: holder_owned.clone(),
                        holders: holders.to_vec(),
                    },
                    observer_inner.as_ref(),
                );
            })
        };
        map.wait(&key, state, cancellation).await?;
        Ok(sync_release(Arc::clone(map), key, mode, holder.clone()))
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
            Ok(release_file) => Ok(WorkspaceLease::new(vec![
                release_file,
                release_shared.consume(),
            ])),
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
        Ok(WorkspaceLease::new(vec![release]))
    }
}

#[async_trait]
impl WorkspaceCoordinator for InProcessCoordinator {
    async fn acquire_file(&self, input: AcquireFileInput) -> Result<WorkspaceLease, CodingError> {
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

    async fn quarantine_workspace(&self, root: &str, evidence: &QuarantineEvidence) {
        let root = canonical_root_key(root).await;
        self.quarantines.lock().expect("quarantine lock").insert(
            root.clone(),
            WorkspaceQuarantine {
                root,
                pids: evidence.pids.clone(),
                holder: evidence.holder.clone(),
                quarantined_at_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|since| since.as_millis() as u64)
                    .unwrap_or_default(),
            },
        );
    }

    async fn workspace_quarantine(
        &self,
        root: &str,
    ) -> Result<Option<WorkspaceQuarantine>, CodingError> {
        let root = canonical_root_key(root).await;
        Ok(self.live_quarantine(&root))
    }

    async fn clear_workspace_quarantine(&self, root: &str) -> Result<bool, CodingError> {
        let root = canonical_root_key(root).await;
        Ok(self
            .quarantines
            .lock()
            .expect("quarantine lock")
            .remove(&root)
            .is_some())
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
                },
            )
            .await;
        let error = coordinator
            .acquire_workspace(AcquireWorkspaceInput {
                root: root.clone(),
                holder: holder("r2", "command", "next"),
                cancellation: None,
                max_wait_ms: Some(120),
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
                },
            )
            .await;
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
            })
            .await
            .expect("clean after recovery");
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

        async fn wait_for(&self, needle: &str) {
            loop {
                if self
                    .events
                    .lock()
                    .expect("observer lock")
                    .iter()
                    .any(|event| event.contains(needle))
                {
                    return;
                }
                self.signal.notified().await;
            }
        }
    }

    impl CoordinatorObserver for RecordingObserver {
        fn on_waiting(&self, wait: &CoordinationWait) {
            let kind = match wait.kind {
                WaitKind::File => "file",
                WaitKind::Workspace => "workspace",
            };
            self.events.lock().expect("observer lock").push(format!(
                "waiting:{kind}:{}:{}",
                wait.waiter.run_id, wait.waiter.tool
            ));
            self.signal.notify_waiters();
        }

        fn on_acquired(&self, wait: &CoordinationWait) {
            self.events.lock().expect("observer lock").push(format!(
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
            }),
        )
        .await
        .expect("workspace must not stay wedged by an abandoned file acquire")
        .expect("workspace exclusive grants");
        reclaim.release().await;
    }
}
