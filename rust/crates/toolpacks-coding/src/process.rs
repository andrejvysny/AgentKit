//! Process-group kill and bounded, tail-keeping command execution.
//!
//! `kill_process_tree` adapted from Pi (https://github.com/earendil-works/pi),
//! v1.4.0, commit 4a080252f06caf63a525d7873c5d94d3c23887e0, path
//! `packages/coding-agent/src/utils/shell.ts`; the bounded runner follows
//! `packages/coding-agent/src/core/tools/bash.ts` (timeout, tail truncation,
//! full log in a temp file).
//! Copyright (c) 2025 Mario Zechner. MIT License.
//! Changes: SIGSTOP the group before SIGKILL so it cannot fork during the
//! kill; no Windows branch (macOS-first); streaming accumulator rewritten.
//! `terminate` is new: marker-based descendant discovery, TERM then KILL,
//! survivor report.
//!
//! Ported from `packages/toolpacks-coding/src/process.ts`.

use std::collections::{HashMap, HashSet};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;
use tokio::time::Instant;

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{ChildStderr, ChildStdout};
use tokio::sync::{Mutex, oneshot};
use tokio_util::sync::CancellationToken;

use crate::coordination::{LeaseHolder, QuarantineEvidence, WorkspaceCoordinator, WorkspaceLease};
use crate::errors::CodingError;
use crate::limits::CodingLimits;
use crate::process_tree::{
    EXEC_ID_ENV, TreeTracker, crate_platform, find_marked_processes_outcome, scan_marked_outcome,
};
use crate::truncate::{TruncationLimits, format_size, truncate_tail};

/// POSIX signal numbers the teardown sequence speaks. On Windows there are
/// no POSIX signals: the constants are distinct sentinels and every send
/// resolves to a no-op, so teardown discovers "cannot kill", reports the
/// group as still alive, and the workspace quarantines instead of pretending
/// cleanup happened. Fail-closed, never fake-success.
#[cfg(unix)]
mod sig {
    pub(super) const SIGSTOP: i32 = libc::SIGSTOP;
    pub(super) const SIGTERM: i32 = libc::SIGTERM;
    pub(super) const SIGCONT: i32 = libc::SIGCONT;
    pub(super) const SIGKILL: i32 = libc::SIGKILL;
}

#[cfg(not(unix))]
mod sig {
    pub(super) const SIGSTOP: i32 = 0;
    pub(super) const SIGTERM: i32 = 1;
    pub(super) const SIGCONT: i32 = 2;
    pub(super) const SIGKILL: i32 = 9;
}

/// Kill `pid` and every process in its group.
#[cfg(unix)]
pub fn kill_process_tree(pid: i32) {
    // group may be gone, or never was one
    unsafe { libc::kill(-pid, libc::SIGSTOP) };
    if unsafe { libc::kill(-pid, libc::SIGKILL) } != 0 {
        // already dead
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
}

/// Windows has no process groups and no `kill`: nothing can be signalled
/// here. Callers observe survivors/UNKNOWN through the liveness probes
/// (which answer "alive" on this platform) and quarantine accordingly.
#[cfg(not(unix))]
pub fn kill_process_tree(_pid: i32) {}

/// One bounded command run.
pub struct RunSpec {
    pub file: String,
    pub args: Vec<String>,
    pub cwd: String,
    pub env: HashMap<String, String>,
    pub timeout_ms: u64,
    pub cancellation: Option<CancellationToken>,
    pub scratch: String,
    pub log_name: String,
    pub limits: CodingLimits,
    /// Stable execution identity `{owner_pid}:{uuid}`. It names BOTH the
    /// `AGENTKIT_EXEC_ID` marker inherited by the whole process tree and
    /// the durable lease/quarantine record, so cleanup evidence names the
    /// exact execution, not a pid that can be recycled (ADR 0020). The
    /// caller mints it before acquiring the workspace lease.
    pub execution_id: String,
    /// Set by [`spawn_owned_execution`]; holds the process-group id once
    /// the child has spawned so a concurrent host shutdown can signal the
    /// group. Construct as `None`.
    pub pgid_slot: Option<Arc<AtomicI64>>,
}

/// The outcome of one bounded command run.
#[derive(Clone, Debug)]
pub struct RunResult {
    /// Tail of the output, already truncated to the limits.
    pub output: String,
    /// True when `output` is not the whole output.
    pub truncated: bool,
    pub notice: String,
    pub log_path: Option<String>,
    pub exit_code: Option<i32>,
    /// Name of the terminating signal ("SIGTERM"), None when it exited.
    pub signal: Option<String>,
    pub timed_out: bool,
    pub aborted: bool,
    pub spawn_error: Option<String>,
    pub teardown: Teardown,
}

/// What the teardown of one command found and had to do.
///
/// KILL POLICY: teardown only ever signals processes that POSITIVELY carry
/// the execution marker (plus the recorded process group). `suspects` —
/// live same-user processes whose `/proc/<pid>/environ` denied reads and
/// which the ownership/age checks could not exclude as foreign — are
/// REPORTED, never killed: they may be entirely unrelated (a gpg-agent
/// class daemon), and killing them would be collateral damage for evidence
/// that only says "might be ours".
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Teardown {
    /// True when SIGTERM was not enough and SIGKILL was sent.
    pub escalated: bool,
    /// Marked pids still alive after SIGKILL (kill failures).
    pub survivors: Vec<i32>,
    /// `Unsupported`: descendants outside the group could not be looked for.
    pub scan: TeardownScan,
    /// Suspect pids (environ-denied, plausibly ours) still alive when
    /// teardown reported. Never signalled — see the kill policy above.
    #[serde(default)]
    pub suspects: Vec<i32>,
    /// The recorded process group still had members when teardown reported,
    /// even though no marked pid could be named inside it (an `env -i`
    /// scrubber, for instance). A group that stays populated after the kill
    /// is a kill failure of the group as a whole.
    #[serde(default)]
    pub group_stuck: bool,
}

impl Teardown {
    /// Cleanup positively established: nothing alive was found.
    pub const fn clean() -> Self {
        Self {
            escalated: false,
            survivors: Vec::new(),
            scan: TeardownScan::Ok,
            suspects: Vec::new(),
            group_stuck: false,
        }
    }

    /// Cleanup not even attemptable: the platform cannot scan and signals
    /// cannot be sent, so nothing about the survivor set was established.
    pub const fn unknown() -> Self {
        Self {
            escalated: false,
            survivors: Vec::new(),
            scan: TeardownScan::Unsupported,
            suspects: Vec::new(),
            group_stuck: false,
        }
    }

    /// The cleanup decision every call site must consume: `Clean` only when
    /// the scan completed and NOTHING was found alive — no marked survivor,
    /// no unresolved environ-denial suspect, no populated recorded group.
    /// Anything else is `Uncertain` with its complete reason set; an empty
    /// survivor list alongside suspects or an unsupported scan is UNKNOWN,
    /// never proof that cleanup completed.
    pub fn outcome(&self) -> CleanupOutcome {
        let uncertainty = Uncertainty {
            survivors: self.survivors.clone(),
            suspects: self.suspects.clone(),
            group_stuck: self.group_stuck,
            scan_unsupported: self.scan == TeardownScan::Unsupported,
        };
        if uncertainty.is_empty() {
            CleanupOutcome::Clean
        } else {
            CleanupOutcome::Uncertain(uncertainty)
        }
    }
}

/// Everything teardown could NOT positively settle about an execution's
/// process tree. Each field is one independent uncertainty class; a
/// cleanup decision may only treat the survivor set as settled when every
/// one of them is empty/false.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Uncertainty {
    /// Marked pids that were signalled and stayed alive (kill failures).
    pub survivors: Vec<i32>,
    /// Live same-user processes whose environ denied reads (EPERM — e.g.
    /// `PR_SET_DUMPABLE=0` children detached into their own session) and
    /// which may be hiding the execution nonce. Reported, NEVER killed.
    pub suspects: Vec<i32>,
    /// The recorded process group stayed populated with no nameable member.
    pub group_stuck: bool,
    /// The platform scan could not run at all: the survivor set is unknown.
    pub scan_unsupported: bool,
}

impl Uncertainty {
    pub fn is_empty(&self) -> bool {
        self.survivors.is_empty()
            && self.suspects.is_empty()
            && !self.group_stuck
            && !self.scan_unsupported
    }

    /// Human-readable reason list for notices/warnings.
    pub fn describe(&self) -> String {
        let mut reasons: Vec<String> = Vec::new();
        if !self.survivors.is_empty() {
            reasons.push(format!(
                "marked survivors {}",
                self.survivors
                    .iter()
                    .map(i32::to_string)
                    .collect::<Vec<_>>()
                    .join(",")
            ));
        }
        if !self.suspects.is_empty() {
            reasons.push(format!(
                "environ-denied suspects {}",
                self.suspects
                    .iter()
                    .map(i32::to_string)
                    .collect::<Vec<_>>()
                    .join(",")
            ));
        }
        if self.group_stuck {
            reasons.push("recorded process group still populated".to_owned());
        }
        if self.scan_unsupported {
            reasons.push("teardown scan unsupported on this platform".to_owned());
        }
        reasons.join("; ")
    }
}

/// The cleanup verdict every cleanup decision site must consume: normal
/// completion, timeout, cancellation, workspace/host drain, crash recovery
/// and the startup sweep. `Clean` is positive evidence only; any surviving
/// uncertainty keeps the workspace blocked — it is never read as success.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CleanupOutcome {
    /// Cleanup positively established: scan completed, nothing alive found.
    Clean,
    /// Cleanup not established; the reasons name what stays unknown.
    Uncertain(Uncertainty),
}

impl CleanupOutcome {
    pub fn is_clean(&self) -> bool {
        matches!(self, CleanupOutcome::Clean)
    }

    /// The uncertainty carried here (empty when `Clean`).
    pub fn uncertainty(&self) -> Uncertainty {
        match self {
            CleanupOutcome::Clean => Uncertainty::default(),
            CleanupOutcome::Uncertain(uncertainty) => uncertainty.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TeardownScan {
    Ok,
    Unsupported,
}

const KILL_GRACE_CAP_MS: u64 = 10_000;
const SURVIVOR_WAIT_MS: u64 = 500;
const POLL_MS: u64 = 25;

#[cfg(unix)]
fn signal_pid(pid: i32, sig: i32) {
    // already gone
    unsafe { libc::kill(pid, sig) };
}

#[cfg(not(unix))]
fn signal_pid(_pid: i32, _sig: i32) {}

#[cfg(unix)]
fn pid_alive(pid: i32) -> bool {
    let rc = unsafe { libc::kill(pid, 0) };
    if rc == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// No portable liveness probe exists; claiming "dead" would retire leases
/// and lift quarantines on live work, so Windows answers "alive" and every
/// cleanup decision stays blocked until positively established.
#[cfg(not(unix))]
fn pid_alive(_pid: i32) -> bool {
    true
}

/// Owner liveness for markers whose `owner_pid` may exceed a real `pid_t`
/// (TS accepts any positive safe integer; `process.kill(pid, 0)` answers
/// ESRCH/EINVAL for absurd values, i.e. "not alive"). Also the liveness
/// probe for workspace quarantine survivor pids.
pub(crate) fn owner_alive(pid: i64) -> bool {
    pid <= i32::MAX as i64 && pid_alive(pid as i32)
}

/// Is the process group led by `pgid` still populated? `kill(-pgid, 0)`
/// answers ESRCH only once every member is gone; EPERM means the group
/// exists but belongs to someone else — conservatively alive.
pub(crate) fn group_alive(pgid: i32) -> bool {
    if pgid <= 0 {
        return false;
    }
    group_populated(pgid)
}

/// Is the process group led by `pgid` still populated? `kill(-pgid, 0)`
/// answers ESRCH only once every member is gone; EPERM means the group
/// exists but belongs to someone else — conservatively alive.
#[cfg(unix)]
fn group_populated(pgid: i32) -> bool {
    // SAFETY: kill(-pgid, 0) performs error checking only, sends no signal.
    unsafe {
        if libc::kill(-pgid, 0) == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    }
}

/// No group probe exists; assume populated (fail-closed, see `pid_alive`).
#[cfg(not(unix))]
fn group_populated(_pgid: i32) -> bool {
    true
}

/// Marker scan that distinguishes "the platform cannot be scanned" from a
/// transient I/O failure under process churn: a single failed pass retries
/// before the caller may treat the survivor set as UNKNOWN. A genuinely
/// unsupported platform returns `None` without any I/O (the platform gate),
/// so the retries cost only the inter-attempt delay there.
async fn marked_processes_reliably(exec_id: &str, spawned_at_ms: u64) -> Option<Vec<i32>> {
    for attempt in 0..3u32 {
        if let Some((pids, suspects)) =
            find_marked_processes_outcome(exec_id, Some(spawned_at_ms)).await
        {
            // A live process whose environ could not be read may be hiding
            // the execution nonce: an empty marks list alongside unresolved
            // suspects is unknown, not positively gone.
            if suspects.is_empty() {
                return Some(pids);
            }
        }
        if attempt + 1 < 3 {
            sleep(50).await;
        }
    }
    None
}

/// SAFE positive-exit proof for a command quarantine (B5). A bare pid probe
/// is NEVER sufficient (pids get recycled). Positive cleanup requires ALL
/// of: (a) every pid the record NAMES is dead (named survivors/suspects
/// are blocking evidence, not decoration); (b) the marker scan succeeds,
/// no live process still carries the execution nonce, and no suspect
/// survives; (c) the recorded process group is provably empty. A scan the
/// platform cannot perform (`None`) is unknown, not empty, and never
/// establishes cleanup; neither does a missing pgid (spawn was never
/// confirmed).
pub(crate) async fn cleanup_positively_established(
    identity: &crate::coordination::CommandCleanupIdentity,
    pids: &[i32],
) -> bool {
    // A pid the quarantine NAMED as its evidence must itself be positively
    // gone: a survivor or suspect listed in the record keeps the record
    // blocked regardless of what the marker scan alone says.
    if pids
        .iter()
        .any(|pid| !pid.is_negative() && crate::process::owner_alive(i64::from(*pid)))
    {
        return false;
    }
    let Some(marked) =
        marked_processes_reliably(&identity.execution_id, identity.spawned_at_ms).await
    else {
        return false;
    };
    if !marked.is_empty() {
        return false;
    }
    match identity.pgid {
        Some(pgid) => !group_alive(pgid),
        None => false,
    }
}

/// Reconcile a crashed run's declared execution (owner pid dead): `Some`
/// lists the processes that still carry the execution nonce (empty =
/// positively gone, retire the intent); `None` means the survivor set is
/// UNKNOWN — the platform cannot scan, or the recorded process group is
/// still populated without a visible marker — and the root must stay
/// blocked.
pub(crate) async fn crashed_execution_survivors(
    intent: &crate::coordination::ExecutionIntent,
) -> Option<Vec<i32>> {
    let (marked, suspects) = find_marked_processes_outcome(
        &intent.identity.execution_id,
        Some(intent.identity.spawned_at_ms),
    )
    .await?;
    if !marked.is_empty() {
        return Some(marked);
    }
    if !suspects.is_empty() {
        // A same-user live process with an unreadable environ could carry
        // the nonce: the survivor set is unknown, the root stays blocked.
        return None;
    }
    match intent.identity.pgid {
        Some(pgid) if group_alive(pgid) => None,
        _ => Some(vec![]),
    }
}

async fn sleep(ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

/// Is the recorded group STILL genuinely populated? A zombie whose parent
/// died counts as a group member (`kill(-pgid, 0)` answers 0) for the
/// moment until its reaper gets it; confirming across several polls tells
/// a transient reap delay under load (cleanup fine) from a member that
/// truly cannot be removed (kill failure: e.g. a zombie whose live parent
/// never reaps it, which must read as uncertainty).
async fn group_populated_sustained(pgid: Option<i32>) -> bool {
    let Some(pgid) = pgid else {
        return false;
    };
    if !pid_alive(-pgid) {
        return false;
    }
    for _ in 0..12 {
        sleep(POLL_MS).await;
        if !pid_alive(-pgid) {
            return false;
        }
    }
    true
}

/// Anything still alive after a wait: a live MARKED pid is conclusive;
/// a merely "populated" group needs the sustained confirmation.
async fn settled(known: &[i32], pgid: Option<i32>) -> bool {
    known.iter().any(|pid| pid_alive(*pid)) || group_populated_sustained(pgid).await
}

/// Recheck live suspects, retaining unresolved suspects and promoting any
/// newly readable execution markers to survivors. A same-user child
/// caught mid-`execve` answers EPERM on
/// environ for a moment (the creds transition) and must not pin a phantom
/// uncertainty onto a clean command; a genuine `PR_SET_DUMPABLE=0` detach
/// stays unreadable across every pass. An unscannable re-check keeps what
/// was seen — unknown stays unknown, never silently dropped.
async fn report_suspects(find: &FindFn, suspects: &[i32], scan: &mut TeardownScan) -> Discovery {
    let alive =
        |pids: &[i32]| -> Vec<i32> { pids.iter().copied().filter(|pid| pid_alive(*pid)).collect() };
    let mut live = alive(suspects);
    let mut marked = Vec::new();
    for _ in 0..2 {
        if live.is_empty() {
            break;
        }
        sleep(50).await;
        match find().await {
            Some(found) => {
                // Readable now does not mean unrelated: a previously
                // unreadable process can now positively expose our marker.
                // It was not part of the signal set, so retain it as a
                // marked survivor instead of silently declaring cleanup.
                live.retain(|pid| found.suspects.contains(pid));
                for pid in found.marked {
                    if !marked.contains(&pid) {
                        marked.push(pid);
                    }
                }
                for pid in found.suspects {
                    if !live.contains(&pid) {
                        live.push(pid);
                    }
                }
            }
            None => {
                *scan = TeardownScan::Unsupported;
                break;
            }
        }
    }
    Discovery {
        marked: alive(&marked),
        suspects: alive(&live),
    }
}

/// What one discovery pass reports for a cleanup decision.
///
/// `marked` are processes POSITIVELY carrying the execution nonce — these
/// are the ones teardown may signal. `suspects` are live same-user
/// processes whose environ denied reads and which the ownership/age checks
/// could not exclude as foreign: they carry UNRESOLVED UNCERTAINTY and are
/// reported, never killed (they may be unrelated). A discovery closure
/// returning `None` means the platform scan could not run at all — the
/// survivor set is unknown, not empty.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Discovery {
    pub marked: Vec<i32>,
    pub suspects: Vec<i32>,
}

/// Injectable process-discovery view for teardown. Production builds it
/// from the marker scan (two-pass union, `agentkit_process_tree`
/// classification is authoritative); tests inject deterministic outcomes.
pub type FindFn = Arc<dyn Fn() -> BoxFuture<'static, Option<Discovery>> + Send + Sync>;

/// The production discovery closure for one execution: the marker scan
/// with its suspect set — suspects are NEVER dropped here, they flow into
/// the cleanup outcome (dropping them is what let a live marked child
/// hiding behind an unreadable environ read as "cleanup completed").
/// `spawned_at_ms` bounds which unreadable same-user processes can
/// plausibly carry the nonce; the darwin tree tracker adds extra marked
/// pids to the union.
fn production_find(
    exec_id: String,
    tracker: Option<Arc<TreeTracker>>,
    spawned_at_ms: u64,
) -> FindFn {
    Arc::new(move || {
        let exec_id = exec_id.clone();
        let tracker = tracker.clone();
        Box::pin(async move {
            let marked = find_marked_processes_outcome(&exec_id, Some(spawned_at_ms)).await;
            let tracked = match &tracker {
                Some(tracker) => tracker.sample().await,
                None => Some(Vec::new()),
            };
            discovery_from_scans(marked, tracked)
        })
    })
}

/// Both required discovery sources must complete. An absent optional tree
/// tracker is supplied as an empty successful sample; it cannot turn a
/// failed marker scan into positive evidence that no descendants remain.
/// A tree sample also cannot prove detached descendants absent on its own.
fn discovery_from_scans(
    marked: Option<(Vec<i32>, Vec<i32>)>,
    tracked: Option<Vec<i32>>,
) -> Option<Discovery> {
    let (marked, suspects) = marked?;
    let tracked = tracked?;
    let mut discovery = Discovery { marked, suspects };
    for pid in tracked {
        if !discovery.marked.contains(&pid) {
            discovery.marked.push(pid);
        }
    }
    Some(discovery)
}

fn absorb(
    known: &mut Vec<i32>,
    suspects: &mut Vec<i32>,
    scan: &mut TeardownScan,
    found: Option<Discovery>,
) {
    match found {
        None => *scan = TeardownScan::Unsupported,
        Some(found) => {
            for pid in found.marked {
                if !known.contains(&pid) {
                    known.push(pid);
                }
            }
            for pid in found.suspects {
                if !suspects.contains(&pid) {
                    suspects.push(pid);
                }
            }
        }
    }
}

/// SIGTERM the group and every MARKED pid, wait `grace_ms`, SIGKILL what is
/// left, then report what still lives — including what could not be looked
/// at. `seen` counts distinct marked pids. Suspects are absorbed into the
/// report but NEVER signalled (see the kill policy on [`Teardown`]).
async fn terminate(pgid: Option<i32>, find: FindFn, grace_ms: u64) -> (Teardown, usize) {
    let mut known: Vec<i32> = Vec::new();
    let mut suspects: Vec<i32> = Vec::new();
    let mut scan = TeardownScan::Ok;
    let signal_all = |sig: i32, known: &[i32]| {
        if let Some(pgid) = pgid {
            signal_pid(-pgid, sig);
        }
        for pid in known {
            signal_pid(*pid, sig);
        }
    };
    let remaining = |known: &[i32]| -> bool {
        (pgid.is_some_and(|pgid| pid_alive(-pgid))) || known.iter().any(|pid| pid_alive(*pid))
    };
    absorb(&mut known, &mut suspects, &mut scan, find().await);
    if known.is_empty() && !remaining(&known) {
        // Nothing killable was found; cleanup is only Clean when the scan
        // completed AND no suspect stayed alive. An empty survivor list
        // alongside suspects or an unsupported scan is UNKNOWN.
        let rechecked = report_suspects(&find, &suspects, &mut scan).await;
        let seen = rechecked.marked.len();
        return (
            Teardown {
                escalated: false,
                survivors: rechecked.marked,
                scan,
                suspects: rechecked.suspects,
                group_stuck: false,
            },
            seen,
        );
    }
    // Freeze first so nothing can fork between discovery and the signal; rescan
    // until the set stops growing. A stopped process only sees SIGTERM once
    // continued, hence the SIGCONT.
    for _ in 0..3 {
        signal_all(sig::SIGSTOP, &known);
        let before = known.len();
        absorb(&mut known, &mut suspects, &mut scan, find().await);
        if known.len() == before {
            break;
        }
    }
    signal_all(sig::SIGTERM, &known);
    signal_all(sig::SIGCONT, &known);
    let deadline = Instant::now() + Duration::from_millis(grace_ms);
    while remaining(&known) && Instant::now() < deadline {
        sleep(POLL_MS).await;
    }
    let escalated = settled(&known, pgid).await;
    if escalated {
        absorb(&mut known, &mut suspects, &mut scan, find().await);
        signal_all(sig::SIGKILL, &known);
        let end = Instant::now() + Duration::from_millis(SURVIVOR_WAIT_MS);
        while remaining(&known) && Instant::now() < end {
            sleep(POLL_MS).await;
        }
    }
    let mut survivors: Vec<i32> = known
        .iter()
        .copied()
        .filter(|pid| pid_alive(*pid))
        .collect();
    if !survivors.is_empty() && scan == TeardownScan::Ok {
        // A zombie nobody reaped still answers kill(pid, 0) but has no
        // environment to find; only a process the scan still sees is a survivor.
        match find().await {
            Some(live) => {
                survivors.retain(|pid| live.marked.contains(pid));
                // This rescan is a discovery too. Preserve newly observed
                // marks and suspects instead of using it only as a filter.
                absorb(&mut known, &mut suspects, &mut scan, Some(live.clone()));
                for pid in live.marked {
                    if pid_alive(pid) && !survivors.contains(&pid) {
                        survivors.push(pid);
                    }
                }
            }
            None => scan = TeardownScan::Unsupported,
        }
    }
    // A group that stays populated after the full kill sequence is a kill
    // failure even when no marked pid names itself inside it. The probe is
    // sustained so a zombie awaiting its reaper under load does not fake a
    // kill failure (and a witness nobody reaps cannot hide).
    let group_stuck = group_populated_sustained(pgid).await;
    let rechecked = report_suspects(&find, &suspects, &mut scan).await;
    for pid in rechecked.marked {
        if !survivors.contains(&pid) {
            survivors.push(pid);
        }
        if !known.contains(&pid) {
            known.push(pid);
        }
    }
    let seen = known.len();
    (
        Teardown {
            escalated,
            survivors,
            scan,
            suspects: rechecked.suspects,
            group_stuck,
        },
        seen,
    )
}

/// Kill commands left behind by agent processes that no longer exist (a crash
/// skips every teardown). Best effort, never throws; returns how many marked
/// processes it found. Call once at boot. A recycled owner pid hides its
/// orphans until that pid is free again.
pub async fn sweep_orphans(grace_ms: Option<u64>) -> usize {
    let find: FindFn = Arc::new(|| {
        Box::pin(async {
            // Outcome-aware: an unscannable platform returns `None` (the
            // teardown records `Unsupported`), never an empty list that
            // reads as "no orphans". Suspects have no attributable owner
            // here (their environ is unreadable, so no marker/owner is
            // visible) and must NOT be killed — the per-execution
            // reconcile pass is what decides their uncertainty.
            scan_marked_outcome(crate_platform(), None)
                .await
                .map(|scan| Discovery {
                    marked: scan
                        .marks
                        .into_iter()
                        .filter(|m| !owner_alive(m.owner_pid))
                        .map(|m| m.pid)
                        .collect(),
                    suspects: vec![],
                })
        })
    });
    let grace = grace_ms.unwrap_or(2000).min(KILL_GRACE_CAP_MS);
    // A panicking teardown task must not read as "nothing found": run the
    // sweep inline (awaited, not spawned-and-forgotten) so the startup
    // recovery really completes before modifying work is admitted.
    match tokio::spawn(terminate(None, find.clone(), grace)).await {
        Ok((_, seen)) => seen,
        Err(_) => terminate(None, find, grace).await.1,
    }
}

/// Keeps a bounded tail in memory; spills the full stream to a scratch file.
struct Tail {
    limits: CodingLimits,
    scratch: String,
    log_name: String,
    chunks: Vec<Vec<u8>>,
    held: usize,
    total: usize,
    spill: Option<tokio::fs::File>,
    spilled: u64,
    dropped: bool,
    log_path: Option<String>,
}

impl Tail {
    fn new(limits: CodingLimits, scratch: String, log_name: String) -> Self {
        Self {
            limits,
            scratch,
            log_name,
            chunks: Vec::new(),
            held: 0,
            total: 0,
            spill: None,
            spilled: 0,
            dropped: false,
            log_path: None,
        }
    }

    async fn push(&mut self, chunk: &[u8]) {
        self.total += chunk.len();
        let hold = self.limits.command_tail_max_bytes * 2;
        if self.spill.is_none() && self.total > hold {
            self.start_spill().await;
        }
        if self.spill.is_some() {
            self.write_spill(chunk).await;
        }
        self.chunks.push(chunk.to_vec());
        self.held += chunk.len();
        while self.held - self.chunks.first().map_or(0, Vec::len) >= hold {
            self.held -= self.chunks.remove(0).len();
            self.dropped = true;
        }
    }

    async fn start_spill(&mut self) {
        let path = join_path(&self.scratch, &self.log_name);
        self.log_path = Some(path.clone());
        self.spill = open_log(&path).await;
        if self.spill.is_some() {
            for chunk in self.chunks.clone() {
                self.write_spill(&chunk).await;
            }
        }
    }

    async fn write_spill(&mut self, chunk: &[u8]) {
        let Some(file) = self.spill.as_mut() else {
            return;
        };
        let room = self
            .limits
            .command_max_log_bytes
            .saturating_sub(self.spilled);
        if room == 0 {
            return;
        }
        let part = if chunk.len() as u64 > room {
            &chunk[..room as usize]
        } else {
            chunk
        };
        self.spilled += part.len() as u64;
        if file.write_all(part).await.is_err() {
            self.spill = None;
        }
    }

    async fn finish(&mut self) -> (String, usize, bool, Vec<u8>) {
        if let Some(file) = self.spill.as_mut() {
            let _ = file.flush().await;
        }
        self.spill = None;
        let buffer: Vec<u8> = self.chunks.concat();
        // Buffer.toString("utf-8") then strip a leading run of replacement
        // chars (a split multi-byte character at the window's start).
        let text = String::from_utf8_lossy(&buffer)
            .trim_start_matches('\u{FFFD}')
            .to_string();
        (text, self.total, self.dropped, buffer)
    }
}

/// `path.join` for posix paths.
fn join_path(dir: &str, name: &str) -> String {
    if dir == "/" {
        format!("/{name}")
    } else if name.starts_with('/') {
        name.to_string()
    } else {
        format!("{}/{}", dir.trim_end_matches('/'), name)
    }
}

async fn open_log(path: &str) -> Option<tokio::fs::File> {
    tokio::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .await
        .ok()
}

async fn write_log(path: &str, buffer: &[u8]) -> std::io::Result<()> {
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .await?;
    file.write_all(buffer).await?;
    file.flush().await
}

fn describe_truncation(
    total: usize,
    shown_lines: usize,
    shown_bytes: usize,
    log_path: Option<&str>,
) -> String {
    let where_ = match log_path {
        Some(path) => format!(" Full output (scratch, outside the workspace): {path}"),
        None => String::new(),
    };
    format!(
        "[Output truncated: showing the last {shown_lines} lines ({} of {}).{where_}]",
        format_size(shown_bytes),
        format_size(total)
    )
}

/// Node-style signal names from the raw terminating signal number.
#[cfg(unix)]
fn signal_name(sig: i32) -> String {
    let name = match sig {
        libc::SIGHUP => "SIGHUP",
        libc::SIGINT => "SIGINT",
        libc::SIGQUIT => "SIGQUIT",
        libc::SIGILL => "SIGILL",
        libc::SIGTRAP => "SIGTRAP",
        libc::SIGABRT => "SIGABRT",
        libc::SIGBUS => "SIGBUS",
        libc::SIGFPE => "SIGFPE",
        libc::SIGKILL => "SIGKILL",
        libc::SIGUSR1 => "SIGUSR1",
        libc::SIGSEGV => "SIGSEGV",
        libc::SIGUSR2 => "SIGUSR2",
        libc::SIGPIPE => "SIGPIPE",
        libc::SIGALRM => "SIGALRM",
        libc::SIGTERM => "SIGTERM",
        libc::SIGCHLD => "SIGCHLD",
        libc::SIGCONT => "SIGCONT",
        libc::SIGSTOP => "SIGSTOP",
        libc::SIGTSTP => "SIGTSTP",
        libc::SIGTTIN => "SIGTTIN",
        libc::SIGTTOU => "SIGTTOU",
        libc::SIGWINCH => "SIGWINCH",
        libc::SIGXCPU => "SIGXCPU",
        libc::SIGXFSZ => "SIGXFSZ",
        _ => return format!("SIG{sig}"),
    };
    name.to_string()
}

/// Windows terminates children by handle, not signal; a synthetic name
/// keeps the run result shape identical without inventing a POSIX table.
#[cfg(not(unix))]
fn signal_name(sig: i32) -> String {
    format!("SIG{sig}")
}

async fn read_out(
    stream: &mut Option<ChildStdout>,
    buf: &mut [u8],
) -> Option<std::io::Result<usize>> {
    match stream.as_mut() {
        Some(s) => Some(s.read(buf).await),
        None => None,
    }
}

async fn read_err(
    stream: &mut Option<ChildStderr>,
    buf: &mut [u8],
) -> Option<std::io::Result<usize>> {
    match stream.as_mut() {
        Some(s) => Some(s.read(buf).await),
        None => None,
    }
}

async fn wait_cancel(token: Option<CancellationToken>) {
    match token {
        Some(token) => token.cancelled().await,
        None => std::future::pending::<()>().await,
    }
}

fn failed_spawn(message: &str) -> RunResult {
    RunResult {
        output: String::new(),
        truncated: false,
        notice: String::new(),
        log_path: None,
        exit_code: None,
        signal: None,
        timed_out: false,
        aborted: false,
        spawn_error: Some(message.to_string()),
        // Nothing ever spawned: there is no process tree left to clean.
        teardown: Teardown::clean(),
    }
}

/// Run one process group to completion, timeout or abort, with the
/// production marker-scan discovery view.
pub async fn run_bounded(spec: RunSpec) -> RunResult {
    run_bounded_with_discovery(spec, None).await
}

/// As [`run_bounded`], with an injectable discovery closure. `None` uses
/// the production marker scan; tests inject deterministic outcomes so the
/// uncertainty path (live suspect, environ PermissionDenied, unsupported
/// scan) can be regressed without prctl or racing the real scheduler.
pub async fn run_bounded_with_discovery(spec: RunSpec, discovery: Option<FindFn>) -> RunResult {
    let tail = Arc::new(Mutex::new(Tail::new(
        spec.limits.clone(),
        spec.scratch.clone(),
        spec.log_name.clone(),
    )));
    // Inherited by every descendant, which is how teardown and the crash sweep
    // find the ones that left the process group. The caller mints the id so
    // the marker and the durable ownership record name the same execution.
    let exec_id = spec.execution_id.clone();
    let mut command = tokio::process::Command::new(&spec.file);
    command
        .args(&spec.args)
        .current_dir(&spec.cwd)
        .env_clear()
        .envs(&spec.env)
        .env(EXEC_ID_ENV, &exec_id)
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(false);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return failed_spawn(&error.to_string()),
    };
    let spawned_at_ms = now_ms();
    let pid: Option<i32> = child.id().map(|p| p as i32);
    // process_group(0): the child leads its own group, so its pid is the pgid
    // a concurrent host shutdown must signal.
    if let (Some(slot), Some(pid)) = (&spec.pgid_slot, pid) {
        slot.store(pid as i64, Ordering::SeqCst);
    }
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();

    // The marker misses Apple-signed binaries on macOS, so sample the tree too.
    let tracker: Option<Arc<TreeTracker>> = if crate_platform() == "darwin" {
        pid.map(|p| Arc::new(TreeTracker::new(p, 100, 1000)))
    } else {
        None
    };
    if let Some(tracker) = &tracker {
        tracker.start();
    }
    // The discovery view must preserve the scan's FULL outcome: marked
    // pids, environ-denial suspects, and "unscannable" (None). Dropping
    // suspects here is what let a live marked child that detached with
    // `PR_SET_DUMPABLE=0` (unreadable environ) read as successful cleanup.
    let find: FindFn = discovery
        .unwrap_or_else(|| production_find(exec_id.clone(), tracker.clone(), spawned_at_ms));

    let mut timed_out = false;
    let mut aborted = false;
    let mut exit_code: Option<i32> = None;
    let mut exit_signal: Option<String> = None;
    let mut exited = false;
    let mut settled = false;
    let mut grace_at: Option<Instant> = None;
    let timeout_at = Instant::now() + Duration::from_millis(spec.timeout_ms);
    let mut timeout_pending = true;
    let mut cancellation = spec.cancellation.clone();
    let mut teardown_run: Option<tokio::task::JoinHandle<Teardown>> = None;

    let start_teardown = |teardown_run: &mut Option<tokio::task::JoinHandle<Teardown>>| {
        if teardown_run.is_none() {
            let find = Arc::clone(&find);
            let grace_ms = spec.limits.command_kill_grace_ms.min(KILL_GRACE_CAP_MS);
            *teardown_run = Some(tokio::spawn(async move {
                terminate(pid, find, grace_ms).await.0
            }));
        }
    };
    // Background children can keep the pipes open after the shell exits.
    // Wait until the pipes have been quiet for a moment (so a loaded machine
    // can still drain them), then sweep the group and stop waiting.
    let arm_grace = |timed_out: bool, aborted: bool, grace_at: &mut Option<Instant>| {
        let ms = if timed_out || aborted {
            spec.limits.command_kill_grace_ms
        } else {
            250
        };
        *grace_at = Some(Instant::now() + Duration::from_millis(ms));
    };
    if cancellation.as_ref().is_some_and(|t| t.is_cancelled()) {
        aborted = true;
        cancellation = None;
        start_teardown(&mut teardown_run);
    }

    let mut out_buf = vec![0u8; 65_536];
    let mut err_buf = vec![0u8; 65_536];
    loop {
        if settled {
            break;
        }
        let grace_deadline = grace_at;
        tokio::select! {
            biased;
            r = read_out(&mut stdout, &mut out_buf), if stdout.is_some() => {
                match r {
                    Some(Ok(n)) if n > 0 => {
                        tail.lock().await.push(&out_buf[..n]).await;
                        if exited && !settled {
                            arm_grace(timed_out, aborted, &mut grace_at);
                        }
                    }
                    _ => {
                        stdout = None;
                        if exited && stderr.is_none() {
                            settled = true;
                        }
                    }
                }
            }
            r = read_err(&mut stderr, &mut err_buf), if stderr.is_some() => {
                match r {
                    Some(Ok(n)) if n > 0 => {
                        tail.lock().await.push(&err_buf[..n]).await;
                        if exited && !settled {
                            arm_grace(timed_out, aborted, &mut grace_at);
                        }
                    }
                    _ => {
                        stderr = None;
                        if exited && stdout.is_none() {
                            settled = true;
                        }
                    }
                }
            }
            status = child.wait(), if !exited => {
                if let Ok(status) = status {
                    exit_code = status.code();
                    #[cfg(unix)]
                    {
                        exit_signal = std::os::unix::process::ExitStatusExt::signal(&status)
                            .map(signal_name);
                    }
                }
                exited = true;
                arm_grace(timed_out, aborted, &mut grace_at);
                if stdout.is_none() && stderr.is_none() {
                    // 'close': exited and both pipes drained.
                    settled = true;
                }
            }
            _ = tokio::time::sleep_until(timeout_at), if timeout_pending => {
                timed_out = true;
                timeout_pending = false;
                start_teardown(&mut teardown_run);
            }
            // `select!` builds every branch future before checking guards,
            // so this must not unwrap; the guard keeps it unpolluted.
            _ = tokio::time::sleep_until(
                grace_deadline.unwrap_or_else(|| Instant::now() + Duration::from_secs(3600)),
            ), if grace_at.is_some() => {
                start_teardown(&mut teardown_run);
                stdout = None;
                stderr = None;
                settled = true;
            }
            _ = wait_cancel(cancellation.clone()), if cancellation.is_some() => {
                aborted = true;
                cancellation = None;
                start_teardown(&mut teardown_run);
            }
        }
    }
    // Reap the child: every settle path means it exited (EOF paths wait for
    // the exit, the grace path only arms after it), and the teardown sweep has
    // now killed anything still in the group.
    let _ = child.wait().await;
    // Sweep stragglers left by a command that exited with children alive.
    let teardown = match teardown_run.take() {
        Some(handle) => match handle.await {
            Ok(teardown) => teardown,
            // A panicking teardown observed NOTHING: honestly unknown,
            // never an empty survivor list that reads as success.
            Err(_) => Teardown::unknown(),
        },
        None => {
            let find = Arc::clone(&find);
            let grace_ms = spec.limits.command_kill_grace_ms.min(KILL_GRACE_CAP_MS);
            terminate(pid, find, grace_ms).await.0
        }
    };
    if let Some(tracker) = &tracker {
        tracker.stop();
    }

    let (text, total, dropped, buffer) = tail.lock().await.finish().await;
    let cut = truncate_tail(
        &text,
        TruncationLimits {
            max_lines: spec.limits.command_tail_max_lines,
            max_bytes: spec.limits.command_tail_max_bytes,
        },
    );
    let kept_all = !dropped && !cut.truncated;
    let mut log_path = tail.lock().await.log_path.clone();
    if !kept_all && log_path.is_none() && !dropped {
        // Everything is still in memory: the model gets the full log's path too.
        let path = join_path(&spec.scratch, &spec.log_name);
        if write_log(&path, &buffer).await.is_ok() {
            log_path = Some(path);
        }
    }
    RunResult {
        output: cut.content,
        truncated: !kept_all,
        notice: if kept_all {
            String::new()
        } else {
            describe_truncation(
                total,
                cut.output_lines,
                cut.output_bytes,
                log_path.as_deref(),
            )
        },
        log_path,
        exit_code,
        signal: exit_signal,
        timed_out,
        aborted,
        spawn_error: None,
        teardown,
    }
}

/// How long cleanup waits for the teardown report's survivors to exit
/// before it must treat cleanup as not positively established.
pub(crate) const SURVIVOR_RECHECK_MS: u64 = 2000;

/// `process.kill(pid, 0)`: alive AND ours. ESRCH (gone) or EPERM (not ours):
/// not ours to wait on.
#[cfg(unix)]
pub(crate) fn is_alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Assume alive: a false "gone" would stop a wait early and declare
/// survivors clean without evidence (fail-closed, see `pid_alive`).
#[cfg(not(unix))]
pub(crate) fn is_alive(_pid: i32) -> bool {
    true
}

/// Pids from `pids` still alive after waiting up to `ms` for them to exit.
pub(crate) async fn wait_for_exit(pids: &[i32], ms: u64) -> Vec<i32> {
    let mut alive: Vec<i32> = pids.to_vec();
    let deadline = Instant::now() + Duration::from_millis(ms);
    loop {
        alive.retain(|pid| is_alive(*pid));
        if alive.is_empty() || Instant::now() >= deadline {
            break;
        }
        sleep(50).await;
    }
    alive
}

/// The once-per-process crash sweep, awaitable (ADR 0020): a crashed agent
/// never ran its teardown, so no modifying work may start new effects before
/// the marked orphans are swept. Process-once semantics live in the `OnceCell`
/// (every caller awaits the same single initialization); infallible and
/// best-effort like the sweep itself, which never throws.
static RECOVERY: LazyLock<tokio::sync::OnceCell<usize>> = LazyLock::new(tokio::sync::OnceCell::new);

/// Await the completion of the one-time startup orphan sweep; returns how many
/// marked orphans it found. Call this from every path that admits new
/// modifying effects (see `run_shell`).
pub async fn startup_recovery() -> usize {
    *RECOVERY
        .get_or_init(|| async { sweep_orphans(None).await })
        .await
}

/// One live driver execution, visible to host shutdown and tests.
#[derive(Clone, Debug)]
pub struct LiveExecution {
    pub execution_id: String,
    /// `None` until the child has actually spawned.
    pub pgid: Option<i32>,
    pub started_at_ms: u64,
    /// The lease holder's label (the command, truncated), for logs.
    pub holder: String,
    /// The canonical workspace root whose lease this execution owns.
    pub root: String,
}

struct LiveRecord {
    pgid_slot: Arc<AtomicI64>,
    started_at_ms: u64,
    holder: String,
    root: String,
}

// Driver executions that have not finished their teardown + lease-release
// chain yet. Process-local, like the marker registry and the in-process
// gates themselves; host shutdown consults it so no child outlives the
// runtime (and tests can prove the driver, not a dropped future's `Drop`,
// performed the release).
static LIVE: LazyLock<StdMutex<HashMap<String, LiveRecord>>> =
    LazyLock::new(|| StdMutex::new(HashMap::new()));

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_millis() as u64)
        .unwrap_or(0)
}

fn live_snapshot() -> Vec<(String, LiveRecord)> {
    LIVE.lock()
        .map(|live| {
            live.iter()
                .map(|(id, record)| (id.clone(), record.clone_live()))
                .collect()
        })
        .unwrap_or_default()
}

impl LiveRecord {
    fn clone_live(&self) -> Self {
        LiveRecord {
            pgid_slot: Arc::clone(&self.pgid_slot),
            started_at_ms: self.started_at_ms,
            holder: self.holder.clone(),
            root: self.root.clone(),
        }
    }
}

/// Driver executions still live: spawned, not yet done with teardown and the
/// lease release.
pub fn live_executions() -> Vec<LiveExecution> {
    live_snapshot()
        .into_iter()
        .map(|(execution_id, record)| LiveExecution {
            pgid: match record.pgid_slot.load(Ordering::SeqCst) {
                0 => None,
                pgid => Some(pgid as i32),
            },
            execution_id,
            started_at_ms: record.started_at_ms,
            holder: record.holder,
            root: record.root,
        })
        .collect()
}

/// Removes the registry entry when the driver finishes (also on unwind), so
/// `drain_executions` and tests can observe cleanup completion.
struct LiveGuard {
    execution_id: String,
}

impl Drop for LiveGuard {
    fn drop(&mut self) {
        if let Ok(mut live) = LIVE.lock() {
            live.remove(&self.execution_id);
        }
    }
}

/// Drain every live driver execution: SIGTERM-then-SIGKILL the process groups
/// of still-running commands through the same `terminate` path the driver
/// itself uses, then await driver completion within a bounded window. Host
/// shutdown uses this so a runtime never exits with marked children alive.
pub async fn drain_executions() {
    drain_live(None).await;
}

/// Drain only the live driver executions of one workspace root. A pack's
/// `shutdown`/`dispose` must drain its own workspace without touching
/// commands another workspace (another pack, another session) is running.
pub async fn drain_workspace(root: &str) {
    let root = crate::paths::canonical_root(root)
        .await
        .unwrap_or_else(|_| root.to_owned());
    drain_live(Some(&root)).await;
}

async fn drain_live(root: Option<&str>) {
    const DRAIN_BUDGET_MS: u64 = 30_000;
    const TERMINATE_GRACE_MS: u64 = 2_000;
    let deadline = Instant::now() + Duration::from_millis(DRAIN_BUDGET_MS);
    let mut terminating: HashSet<String> = HashSet::new();
    let mut handles = Vec::new();
    loop {
        let live: Vec<(String, Arc<AtomicI64>, u64)> = live_snapshot()
            .into_iter()
            .filter(|(_, record)| root.is_none_or(|r| record.root == r))
            .map(|(execution_id, record)| (execution_id, record.pgid_slot, record.started_at_ms))
            .collect();
        if live.is_empty() {
            break;
        }
        for (execution_id, pgid_slot, started_at_ms) in live {
            let pgid = pgid_slot.load(Ordering::SeqCst);
            // Not spawned yet: the driver will signal it through its own
            // pump; waiting for the registry entry to disappear covers it.
            if pgid <= 0 || terminating.contains(&execution_id) {
                continue;
            }
            terminating.insert(execution_id.clone());
            // Same outcome-preserving discovery as the driver: the drain
            // must see suspects and scan-unsupported too, never a lossy
            // marked-only list that makes cleanup look settled.
            let find: FindFn = production_find(execution_id.clone(), None, started_at_ms);
            handles.push(tokio::spawn(async move {
                terminate(Some(pgid as i32), find, TERMINATE_GRACE_MS).await;
            }));
        }
        if Instant::now() >= deadline {
            break;
        }
        sleep(POLL_MS).await;
    }
    for handle in handles {
        let _ = handle.await;
    }
}

/// What an owned execution's driver established about its cleanup.
#[derive(Debug)]
pub struct OwnedRunOutcome {
    /// The bounded run's result (output, exit status, teardown report).
    pub run: RunResult,
    /// Pids still alive when cleanup was judged: the workspace stays
    /// quarantined for these.
    pub stragglers: Vec<i32>,
    /// The driver's explicit cleanup verdict — the value EVERY cleanup
    /// decision site must consume. `Clean` is positive evidence only;
    /// `Uncertain` keeps the workspace quarantined with durable intent.
    pub cleanup: CleanupOutcome,
    /// The quarantine publication failed, so the lease was left blocking the
    /// gate instead of being released (and the durable intent was kept).
    pub quarantine_error: Option<CodingError>,
}

/// Handle on one owned driver execution. Dropping it does NOT stop the
/// driver — that is the whole point; it only stops this caller from
/// observing the result.
pub struct OwnedExecution {
    execution_id: String,
    completion: oneshot::Receiver<OwnedRunOutcome>,
}

impl OwnedExecution {
    pub fn execution_id(&self) -> &str {
        &self.execution_id
    }

    /// Await the driver's outcome. A caller that never gets here (a tool
    /// future dropped by the core harness on budget expiry) leaves the
    /// driver to finish teardown, the survivor recheck, quarantine and the
    /// lease release on its own.
    pub async fn await_result(self) -> OwnedRunOutcome {
        match self.completion.await {
            Ok(outcome) => outcome,
            Err(_) => OwnedRunOutcome {
                // The driver died before reporting (panic / runtime teardown):
                // honestly unqualified cleanup, and shutdown/sweep own the
                // remaining recovery.
                run: RunResult {
                    output: String::new(),
                    truncated: false,
                    notice: String::new(),
                    log_path: None,
                    exit_code: None,
                    signal: None,
                    timed_out: false,
                    aborted: false,
                    spawn_error: Some("the execution driver died before reporting".to_owned()),
                    teardown: Teardown::unknown(),
                },
                stragglers: vec![],
                cleanup: Teardown::unknown().outcome(),
                quarantine_error: None,
            },
        }
    }
}

/// Run `spec` on a detached driver task that OWNS the workspace lease until
/// cleanup is positively established.
///
/// The whole spawn -> pump -> teardown -> reap -> survivor-recheck ->
/// quarantine -> lease-release chain lives inside one `tokio::spawn`; the
/// `JoinHandle` is deliberately dropped (dropping a handle detaches a task,
/// it never cancels it), and the lease is moved into the task, never held by
/// the awaitable tool future. This is the B1 fix: the core harness completes
/// its timeout `select!` by DROPPING the tool future, and a lease held by
/// that future would hit `WorkspaceLease::drop`'s deferred release the very
/// same instant — releasing the gate while the child still ran and skipping
/// the survivor recheck and the quarantine entirely. With the driver, a
/// dropped caller can neither release the gate ahead of cleanup nor suppress
/// quarantine; it only stops listening.
pub fn spawn_owned_execution(
    spec: RunSpec,
    lease: WorkspaceLease,
    coordinator: Arc<dyn WorkspaceCoordinator>,
    root: String,
    holder: LeaseHolder,
) -> OwnedExecution {
    spawn_owned_execution_with_discovery(spec, lease, coordinator, root, holder, None)
}

/// As [`spawn_owned_execution`], with an injectable discovery closure for
/// the driver's teardown (`None` = the production marker scan). Tests use
/// this to regress the assessor scenario deterministically: no survivors,
/// a live environ-denied suspect, and the cleanup decision that must
/// follow — quarantine + retained durable intent, never a "clean" retire.
pub fn spawn_owned_execution_with_discovery(
    mut spec: RunSpec,
    lease: WorkspaceLease,
    coordinator: Arc<dyn WorkspaceCoordinator>,
    root: String,
    holder: LeaseHolder,
    discovery: Option<FindFn>,
) -> OwnedExecution {
    let execution_id = spec.execution_id.clone();
    let pgid_slot = Arc::new(AtomicI64::new(0));
    spec.pgid_slot = Some(Arc::clone(&pgid_slot));
    let driver_slot = Arc::clone(&pgid_slot);
    if let Ok(mut live) = LIVE.lock() {
        live.insert(
            execution_id.clone(),
            LiveRecord {
                pgid_slot,
                started_at_ms: now_ms(),
                holder: holder.label.clone(),
                root: root.clone(),
            },
        );
    }
    let (sender, completion) = oneshot::channel();
    let driver_id = execution_id.clone();
    let driver_exec = execution_id.clone();
    let watcher_coordinator = Arc::clone(&coordinator);
    let watcher_root = root.clone();
    let watcher_holder = holder.clone();
    let watcher_id = execution_id.clone();
    let spawned_at_ms = now_ms();
    let watcher_slot = Arc::clone(&driver_slot);
    tokio::spawn(async move {
        // Upgrade the pre-spawn intent once the child exists: same nonce,
        // now with the confirmed process group and spawn time. Best effort
        // by design — the pre-spawn intent already blocks reconciliation;
        // a failed upgrade only delays the pgid annotation.
        for _ in 0..200 {
            let pgid = watcher_slot.load(Ordering::SeqCst);
            if pgid > 0 {
                let _ = watcher_coordinator
                    .record_execution_intent(&crate::coordination::ExecutionIntent {
                        root: watcher_root,
                        identity: crate::coordination::CommandCleanupIdentity {
                            execution_id: watcher_id,
                            owner_pid: i64::from(std::process::id()),
                            pgid: Some(pgid as i32),
                            spawned_at_ms,
                        },
                        holder: watcher_holder,
                        spawned: true,
                    })
                    .await;
                return;
            }
            tokio::time::sleep(Duration::from_millis(POLL_MS)).await;
        }
    });
    tokio::spawn(async move {
        // Unregisters on completion (or unwind): the drain loop and tests
        // read this as "the driver is done with cleanup".
        let _live = LiveGuard {
            execution_id: driver_id,
        };
        let run = run_bounded_with_discovery(spec, discovery).await;
        let stragglers = wait_for_exit(&run.teardown.survivors, SURVIVOR_RECHECK_MS).await;
        let pgid = match driver_slot.load(Ordering::SeqCst) {
            0 => None,
            pgid => Some(pgid as i32),
        };
        // Fold the survivor recheck into the teardown verdict: survivors
        // that positively exited are resolved; survivors that stayed alive
        // are kill failures, and SUSPECTS resolve only through their own
        // liveness probe — an empty survivor list NEVER cancels a live
        // suspect or an unsupported scan (that collapse was the assessor
        // defect: `survivors=[]` beside a live suspect read as success).
        let mut uncertainty = run.teardown.outcome().uncertainty();
        uncertainty.survivors = stragglers.clone();
        uncertainty.suspects.retain(|pid| pid_alive(*pid));
        let cleanup = if uncertainty.is_empty() {
            CleanupOutcome::Clean
        } else {
            CleanupOutcome::Uncertain(uncertainty)
        };
        let mut lease = Some(lease);
        let mut quarantine_error = None;
        if let CleanupOutcome::Uncertain(uncertainty) = &cleanup {
            // Cleanup is not positively established (ADR 0020): the lease may
            // be released (a wedge with no recovery path helps nobody), but
            // the workspace stays quarantined — every later acquisition
            // blocks until the evidence resolves or recovery is recorded.
            // Name every piddable reason (kill failures + suspects) in the
            // evidence; an empty list means the uncertainty is unnameable
            // (unsupported scan), which stays blocked just the same. If the
            // blocking evidence itself cannot be published, the lease stays
            // held: live processes must never be handed a blind workspace.
            // `forget` skips `WorkspaceLease::drop`, whose deferred release
            // would otherwise undo exactly that.
            let mut pids = uncertainty.survivors.clone();
            for pid in &uncertainty.suspects {
                if !pids.contains(pid) {
                    pids.push(*pid);
                }
            }
            let identity = crate::coordination::CommandCleanupIdentity {
                execution_id: driver_exec.clone(),
                owner_pid: i64::from(std::process::id()),
                pgid,
                spawned_at_ms,
            };
            if let Err(error) = coordinator
                .quarantine_execution(&root, &QuarantineEvidence { pids, holder }, &identity)
                .await
            {
                quarantine_error = Some(error);
                if let Some(lease) = lease.take() {
                    std::mem::forget(lease);
                }
            }
        }
        // The durable intent is retired ONLY on a positively clean cleanup.
        // Uncertain cleanup keeps the intent alongside the quarantine —
        // quarantine alone must be durable too (the in-process coordinator
        // persists it through the intent journal), and neither half may be
        // dropped while recovery has not resolved the uncertainty. A failed
        // quarantine publication likewise keeps the intent — and the lease.
        if cleanup.is_clean() && quarantine_error.is_none() {
            let _ = coordinator
                .clear_execution_intent(&root, &driver_exec)
                .await;
        }
        if let Some(mut lease) = lease {
            lease.release().await;
        }
        // A dropped receiver (caller gone) must not skip anything above: the
        // send is the driver's last act, never a precondition of it.
        let _ = sender.send(OwnedRunOutcome {
            run,
            stragglers,
            cleanup,
            quarantine_error,
        });
    });
    OwnedExecution {
        execution_id,
        completion,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::CodingLimits;
    use std::process::id as process_id;

    #[test]
    fn incomplete_discovery_cannot_be_completed_by_an_optional_tracker() {
        assert!(discovery_from_scans(None, Some(vec![])).is_none());
        assert!(discovery_from_scans(None, Some(vec![123])).is_none());
        assert!(discovery_from_scans(Some((vec![], vec![])), None).is_none());
        let complete = discovery_from_scans(Some((vec![123], vec![456])), Some(vec![123, 789]))
            .expect("both required sources completed");
        assert_eq!(complete.marked, vec![123, 789]);
        assert_eq!(complete.suspects, vec![456]);
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "agentkit-process-{}-{tag}-{}",
            process_id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn spec_for(scratch: &str, log_name: &str, command: &str) -> RunSpec {
        RunSpec {
            file: "/bin/sh".to_owned(),
            args: vec!["-c".to_owned(), command.to_owned()],
            cwd: std::env::temp_dir().to_string_lossy().into_owned(),
            env: HashMap::from([
                ("PATH".to_owned(), "/usr/bin:/bin".to_owned()),
                ("HOME".to_owned(), "/tmp".to_owned()),
            ]),
            timeout_ms: 15_000,
            cancellation: None,
            scratch: scratch.to_owned(),
            log_name: log_name.to_owned(),
            limits: CodingLimits::default(),
            execution_id: format!("{}:{}", process_id(), uuid::Uuid::new_v4()),
            pgid_slot: None,
        }
    }

    #[test]
    fn kill_process_tree_on_a_dead_pid_does_not_throw() {
        kill_process_tree(2_000_000_000);
    }

    #[test]
    fn describe_truncation_matches_the_ts_text() {
        assert_eq!(
            describe_truncation(50_000, 2000, 40_960, Some("/s/log.txt")),
            "[Output truncated: showing the last 2000 lines (40.0KB of 48.8KB). Full output (scratch, outside the workspace): /s/log.txt]"
        );
        assert_eq!(
            describe_truncation(10, 2, 10, None),
            "[Output truncated: showing the last 2 lines (10B of 10B).]"
        );
    }

    #[tokio::test]
    async fn tail_keeps_only_the_bounded_window_and_spills() {
        let dir = temp_dir("tail");
        let limits = CodingLimits {
            command_tail_max_bytes: 64,
            command_max_log_bytes: 1000,
            ..CodingLimits::default()
        };
        let mut tail = Tail::new(
            limits,
            dir.to_string_lossy().into_owned(),
            "out.log".to_owned(),
        );
        // hold = 128 bytes; push 10 x 40 = 400.
        for i in 0..10u8 {
            tail.push(vec![b'a' + i; 40].as_slice()).await;
        }
        assert_eq!(tail.total, 400);
        assert!(tail.dropped);
        // Trim invariant (as in TS): held - firstChunk < hold, so the window
        // ends up below `hold + chunkSize`.
        assert!(tail.held < 168);
        let (text, total, dropped, buffer) = tail.finish().await;
        assert_eq!(total, 400);
        assert!(dropped);
        assert_eq!(text.len(), tail.held);
        assert_eq!(buffer.len(), tail.held);
        // The full stream went to the log, under the cap.
        let log = dir.join("out.log");
        assert_eq!(std::fs::read(&log).unwrap().len(), 400);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&log).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn tail_caps_the_log_at_max_log_bytes() {
        let dir = temp_dir("tail-cap");
        let limits = CodingLimits {
            command_tail_max_bytes: 64,
            command_max_log_bytes: 100,
            ..CodingLimits::default()
        };
        let mut tail = Tail::new(
            limits,
            dir.to_string_lossy().into_owned(),
            "out.log".to_owned(),
        );
        for _ in 0..20 {
            tail.push(&[b'x'; 40]).await;
        }
        tail.finish().await;
        assert_eq!(std::fs::read(dir.join("out.log")).unwrap().len(), 100);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn tail_strips_leading_replacement_chars() {
        let dir = temp_dir("tail-fffd");
        let mut tail = Tail::new(
            CodingLimits::default(),
            dir.to_string_lossy().into_owned(),
            "out.log".to_owned(),
        );
        tail.push("\u{FFFD}\u{FFFD}hello".as_bytes()).await;
        let (text, ..) = tail.finish().await;
        assert_eq!(text, "hello");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn runs_reports_exit_code_and_keeps_small_output_whole() {
        let dir = temp_dir("run-ok");
        let result = run_bounded(spec_for(
            &dir.to_string_lossy(),
            "cmd.log",
            "echo hello; echo oops >&2; exit 3",
        ))
        .await;
        assert_eq!(result.output, "hello\noops\n");
        assert!(!result.truncated);
        assert_eq!(result.notice, "");
        assert_eq!(result.exit_code, Some(3));
        assert_eq!(result.signal, None);
        assert!(!result.timed_out && !result.aborted);
        assert_eq!(result.spawn_error, None);
        assert_eq!(result.log_path, None);
        assert_eq!(result.teardown.scan, TeardownScan::Ok);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn killed_by_signal_reports_the_name() {
        let dir = temp_dir("run-sig");
        let result = run_bounded(spec_for(&dir.to_string_lossy(), "cmd.log", "kill -9 $$")).await;
        assert_eq!(result.exit_code, None);
        assert_eq!(result.signal.as_deref(), Some("SIGKILL"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn spawn_failure_reports_spawn_error() {
        let dir = temp_dir("run-spawn");
        let mut spec = spec_for(&dir.to_string_lossy(), "cmd.log", "true");
        spec.file = "/nonexistent/agentkit-binary".to_owned();
        let result = run_bounded(spec).await;
        assert_eq!(result.output, "");
        assert!(!result.truncated);
        assert!(result.spawn_error.is_some());
        assert_eq!(result.exit_code, None);
        assert_eq!(result.teardown, Teardown::clean());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn truncation_keeps_a_tail_notice_and_the_full_log() {
        let dir = temp_dir("run-trunc");
        let scratch = dir.to_string_lossy().into_owned();
        let result = run_bounded(spec_for(&scratch, "cmd.log", "seq 1 5000")).await;
        assert!(result.truncated);
        assert!(result.output.ends_with("4999\n5000"));
        assert!(!result.output.starts_with("1\n"));
        assert!(
            result
                .notice
                .starts_with("[Output truncated: showing the last 2000 lines (")
        );
        let log_path = result.log_path.clone().unwrap();
        assert!(result.notice.contains(&log_path));
        assert!(log_path.starts_with(&scratch));
        let full = std::fs::read_to_string(&log_path).unwrap();
        assert!(full.starts_with("1\n2\n"));
        assert!(full.trim_end().ends_with("5000"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&log_path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn truncated_but_unspilled_output_still_gets_a_log() {
        // Truncated by lines only, nothing dropped from memory: the full log
        // is written at the end so the model can still read it.
        let dir = temp_dir("run-linecut");
        let scratch = dir.to_string_lossy().into_owned();
        let mut spec = spec_for(&scratch, "cmd.log", "seq 1 30");
        spec.limits.command_tail_max_lines = 5;
        spec.limits.command_tail_max_bytes = 50 * 1024;
        let result = run_bounded(spec).await;
        assert!(result.truncated);
        assert_eq!(result.output, "26\n27\n28\n29\n30");
        assert!(result.output.len() < 30 * 4);
        let log_path = result.log_path.clone().unwrap();
        assert_eq!(
            std::fs::read_to_string(&log_path).unwrap(),
            (1..=30).map(|i| format!("{i}\n")).collect::<String>()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn timeout_kills_the_whole_process_group() {
        let dir = temp_dir("run-timeout");
        let mut spec = spec_for(
            &dir.to_string_lossy(),
            "cmd.log",
            "sleep 7101 & sleep 7102 & wait",
        );
        spec.timeout_ms = 500;
        spec.limits.command_kill_grace_ms = 300;
        let result = run_bounded(spec).await;
        assert!(result.timed_out);
        assert_eq!(result.teardown.scan, TeardownScan::Ok);
        // The group is gone: signalling it fails.
        if let Some(pid) = result.teardown.survivors.first() {
            panic!("survivor {pid}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn abort_kills_and_reports() {
        let dir = temp_dir("run-abort");
        let token = CancellationToken::new();
        let killer = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            killer.cancel();
        });
        let mut spec = spec_for(
            &dir.to_string_lossy(),
            "cmd.log",
            "sleep 7103 & sleep 7104 & wait",
        );
        spec.cancellation = Some(token);
        let result = run_bounded(spec).await;
        assert!(result.aborted);
        assert!(!result.timed_out);
        assert!(result.teardown.survivors.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_background_child_left_behind_is_swept() {
        let dir = temp_dir("run-bg");
        let result = run_bounded(spec_for(
            &dir.to_string_lossy(),
            "cmd.log",
            "sleep 7105 >/dev/null 2>&1 & echo started",
        ))
        .await;
        assert!(result.output.contains("started"));
        assert!(result.teardown.survivors.is_empty());
        // Give the sweep a moment, then check the marked sleeper is gone.
        for _ in 0..100 {
            if !any_cmdline_contains("7105") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(!any_cmdline_contains("7105"));
        std::fs::remove_dir_all(&dir).ok();
    }

    fn any_cmdline_contains(needle: &str) -> bool {
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return false;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !name.chars().all(|c| c.is_ascii_digit()) {
                continue;
            }
            if let Ok(raw) = std::fs::read(format!("/proc/{name}/cmdline")) {
                let text = String::from_utf8_lossy(&raw);
                if text.contains(needle) {
                    return true;
                }
            }
        }
        false
    }

    #[tokio::test]
    async fn sweep_orphans_kills_a_marked_process_whose_owner_is_dead() {
        // A dead owner pid: far above any plausible live pid on this box.
        let mut child = tokio::process::Command::new("/bin/sleep")
            .arg("300")
            .env("AGENTKIT_EXEC_ID", "2000000000:orphan-test")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = child.id().unwrap() as i32;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(pid_alive(pid));
        let seen = sweep_orphans(Some(500)).await;
        assert!(seen >= 1);
        // Reap it so `pid_alive` does not see a zombie.
        let _ = tokio::time::timeout(Duration::from_secs(3), child.wait()).await;
        assert!(!pid_alive(pid));
    }

    #[tokio::test]
    async fn sweep_orphans_leaves_a_marked_process_whose_owner_is_alive() {
        let mut child = tokio::process::Command::new("/bin/sleep")
            .arg("300")
            .env("AGENTKIT_EXEC_ID", format!("{}:live-test", process_id()))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = child.id().unwrap() as i32;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(pid_alive(pid));
        sweep_orphans(Some(200)).await;
        assert!(pid_alive(pid));
        unsafe { libc::kill(pid, libc::SIGKILL) };
        let _ = child.wait().await;
    }

    #[tokio::test]
    async fn graceful_child_exits_on_sigterm_without_escalation() {
        let dir = temp_dir("run-polite");
        let mut spec = spec_for(
            &dir.to_string_lossy(),
            "cmd.log",
            "sleep 7106 & echo $! > /dev/null; wait",
        );
        spec.timeout_ms = 500;
        spec.limits.command_kill_grace_ms = 1000;
        let result = run_bounded(spec).await;
        assert!(result.timed_out);
        // SIGTERM sufficed: no escalation, nothing survived, and the
        // outcome is positively Clean (no suspects, group drained).
        assert_eq!(result.teardown, Teardown::clean());
        assert!(result.teardown.outcome().is_clean());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn stubborn_child_is_sigkilled_after_the_grace() {
        let dir = temp_dir("run-stubborn");
        let mut spec = spec_for(
            &dir.to_string_lossy(),
            "cmd.log",
            "trap '' TERM; sleep 7107 & wait",
        );
        spec.timeout_ms = 500;
        spec.limits.command_kill_grace_ms = 300;
        let result = run_bounded(spec).await;
        assert!(result.timed_out);
        let mut expected = Teardown::clean();
        expected.escalated = true;
        assert_eq!(result.teardown, expected);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn terminate_reports_injected_suspects_and_never_kills_them() {
        // The kill policy, pinned: a suspect (environ-denied, plausibly
        // ours) is REPORTED as uncertainty and NEVER signalled — it may be
        // an unrelated daemon. The discovery closure here is the same
        // injection seam the assessor scenario regression uses.
        let mut child = tokio::process::Command::new("/bin/sleep")
            .arg("300")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = child.id().unwrap() as i32;
        let suspect = pid;
        let find: FindFn = Arc::new(move || {
            Box::pin(async move {
                Some(Discovery {
                    marked: vec![],
                    suspects: vec![suspect],
                })
            })
        });
        let (teardown, seen) = terminate(None, find, 100).await;
        assert_eq!(seen, 0, "suspects are not killable discoveries");
        assert_eq!(teardown.suspects, vec![pid]);
        assert!(teardown.survivors.is_empty());
        let outcome = teardown.outcome();
        assert!(
            matches!(&outcome, crate::process::CleanupOutcome::Uncertain(u) if u.suspects == vec![pid]),
            "a live suspect must read as Uncertain, not Clean: {outcome:?}"
        );
        assert!(!outcome.is_clean());
        // Not killed, not even signalled: still alive after teardown.
        assert!(pid_alive(pid));
        unsafe { libc::kill(pid, libc::SIGKILL) };
        let _ = child.wait().await;
    }

    #[tokio::test]
    async fn terminate_unknown_scan_is_uncertain_not_clean() {
        // A scan the platform cannot run returns `None`: an empty survivor
        // list must never be read as successful cleanup.
        let find: FindFn = Arc::new(|| Box::pin(async { None }));
        let (teardown, seen) = terminate(Some(2_000_000_000), find, 50).await;
        assert_eq!(seen, 0);
        assert_eq!(teardown.scan, TeardownScan::Unsupported);
        assert!(teardown.survivors.is_empty());
        let outcome = teardown.outcome();
        assert!(
            matches!(&outcome, crate::process::CleanupOutcome::Uncertain(u) if u.scan_unsupported),
            "an unsupported scan is UNKNOWN, never Clean: {outcome:?}"
        );
    }
}
