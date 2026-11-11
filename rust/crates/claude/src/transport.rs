//! Owned streaming process transport with bounded shutdown (AGENTKIT-110).
//!
//! [`OwnedProcessSupervisor`] realizes [`agentkit_native::ProcessSupervisor`]:
//! typed argv, explicit cwd, a scrubbed additive environment, readers that
//! start immediately at spawn (so a child that waits for its prompt before
//! emitting anything can never deadlock against input delivery), bounded
//! framing/backpressure, generation-bound handles, and group-scoped
//! interrupt escalation SIGINT → SIGTERM → SIGKILL with an honest
//! [`InterruptReceipt`]. Normal completion reconciles the owned process
//! group too (review round-4 F5): a leader that exits while its group keeps
//! running is not a finished attempt, so `wait` sweeps and VERIFIES the
//! group empty before finalization disarms cleanup.

use std::collections::{HashMap, VecDeque};
#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::ChildStdin;
use tokio::sync::{Mutex as AsyncMutex, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use agentkit_native::{
    FrameLine, InterruptPolicy, InterruptReceipt, LaunchPlan, NativeError, OwnedChildHandle,
    ProcessExit, ProcessSupervisor, ResourceLimits, TransportFault,
};

/// Environment key that would let the official CLI resume an interrupted
/// turn from ambient state. Always scrubbed from the child environment for
/// this scope: interrupted-turn resume is disabled.
const INTERRUPTED_TURN_RESUME_ENV: &str = "CLAUDE_CODE_RESUME_INTERRUPTED_TURN";

/// Credential environment families that must never reach the vendor CLI.
/// A desktop host can carry provider/API/OAuth tokens in its ambient
/// environment; the unmodified CLI would see them and an ambient token can
/// silently mutate the vendor's auth and billing route — the exact route
/// observation from `auth status` cannot capture this (AGENTKIT-109/116).
/// Ambient matches are removed from every spawned process (execution and
/// discovery probes); the pinned plan environment is applied afterwards.
pub(crate) const CREDENTIAL_ENV_PREFIXES: &[&str] = &[
    "ANTHROPIC_",
    "CLAUDE_CODE_",
    "AWS_BEARER_TOKEN_BEDROCK",
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "GOOGLE_OAUTH_ACCESS_TOKEN",
    "AZURE_OPENAI_API_KEY",
];

/// Ambient environment keys that belong to a credential family.
pub(crate) fn credential_env_keys() -> Vec<std::ffi::OsString> {
    std::env::vars_os()
        .map(|(key, _)| key)
        .filter(|key| {
            key.to_str()
                .is_some_and(|key| CREDENTIAL_ENV_PREFIXES.iter().any(|p| key.starts_with(p)))
        })
        .collect()
}

const SIGINT: i32 = 2;
const SIGTERM: i32 = 15;
const SIGKILL: i32 = 9;

/// Exit facts captured once by the reaper task.
#[derive(Clone, Copy, Debug)]
struct RawExit {
    exit_code: Option<i32>,
    signal: Option<i32>,
}

/// State shared between the supervisor registry, the live handle and drain:
/// identity, termination facts and the cached interrupt receipt.
struct SharedChild {
    pid: u32,
    /// We always spawn with `process_group(0)`, so pgid == pid and group
    /// kills can never target an arbitrary foreign process tree.
    pgid: u32,
    generation: u64,
    /// The attempt's REAL configured limits, kept beside the registry's
    /// weak handle so supervisor-driven cleanup (terminate_owned /
    /// shutdown drain) honours operator grace timeouts instead of
    /// substituting defaults (review round-3 F3).
    limits: ResourceLimits,
    exit: Mutex<Option<RawExit>>,
    interrupt_requested: AtomicBool,
    interrupt_policy: Mutex<Option<String>>,
    cached_receipt: Mutex<Option<InterruptReceipt>>,
}

impl SharedChild {
    fn cached(&self) -> Option<InterruptReceipt> {
        self.cached_receipt.lock().unwrap().clone()
    }

    fn store_receipt(&self, receipt: &InterruptReceipt) {
        *self.cached_receipt.lock().unwrap() = Some(receipt.clone());
    }

    fn exit_now(&self) -> Option<RawExit> {
        *self.exit.lock().unwrap()
    }
}

/// OS process ownership port implementation (AGENTKIT-110). Send + Sync via
/// interior mutability; every live child is registered by pid so a
/// host-shutdown drain can reach it even if its handle went stale.
pub struct OwnedProcessSupervisor {
    next_generation: AtomicU64,
    live: Arc<Mutex<HashMap<u32, std::sync::Weak<SharedChild>>>>,
    /// Review round-4 F5: the honest descendant-cleanup outcome of the most
    /// recent NORMAL completion per pid. [`ProcessExit`] is the shared port
    /// type and cannot carry it, so the supervisor records it for the
    /// attempt identity; bounded, newest wins.
    cleanup_reports: Arc<Mutex<HashMap<u32, (u64, InterruptReceipt)>>>,
}

impl Default for OwnedProcessSupervisor {
    fn default() -> Self {
        Self::new()
    }
}

impl OwnedProcessSupervisor {
    pub fn new() -> Self {
        OwnedProcessSupervisor {
            next_generation: AtomicU64::new(0),
            live: Arc::new(Mutex::new(HashMap::new())),
            cleanup_reports: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Review round-4 F5: descendant-cleanup honesty for a normally
    /// completed attempt. `None` means no normal-completion finalize is
    /// recorded for that exact `(pid, generation)` — which is NOT a claim
    /// that the group was clean. `cleanup_uncertain` on the receipt is the
    /// surface: set when the owned process group could not be VERIFIED
    /// empty after the leader exited. This is the concrete-type read-back;
    /// consumers holding `dyn ProcessSupervisor` use the trait method of
    /// the same purpose, [`ProcessSupervisor::descendant_cleanup`], which
    /// delegates here.
    #[must_use]
    pub fn descendant_cleanup_report(&self, pid: u32, generation: u64) -> Option<InterruptReceipt> {
        self.cleanup_reports
            .lock()
            .unwrap()
            .get(&pid)
            .filter(|(recorded, _)| *recorded == generation)
            .map(|(_, receipt)| receipt.clone())
    }
}

fn fault(code: &str, message: impl Into<String>) -> TransportFault {
    TransportFault {
        code: code.to_owned(),
        message: message.into(),
    }
}

fn policy_label(policy: InterruptPolicy) -> &'static str {
    match policy {
        InterruptPolicy::UserStop => "userStop",
        InterruptPolicy::HostShutdown => "hostShutdown",
        InterruptPolicy::Quarantine => "quarantine",
    }
}

/// Send a signal to our own process group only. The recorded pgid equals
/// the child pid (process_group(0)); we refuse privileged/self targets.
fn group_kill(pgid: u32, sig: i32) -> Result<(), String> {
    if pgid <= 1 || pgid > i32::MAX as u32 || pgid == std::process::id() {
        return Err(format!("refusing to signal process group {pgid}"));
    }
    // SAFETY: kill receives only an owned, validated process-group ID.
    let rc = unsafe { libc::kill(-(pgid as i32), sig) };
    if rc == 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::ESRCH) {
        // Group already gone: nothing to kill, not an uncertainty.
        return Ok(());
    }
    Err(format!("kill(-{pgid}, {sig}) failed: {err}"))
}

fn kernel_group_absent(pgid: u32) -> bool {
    if pgid <= 1 || pgid > i32::MAX as u32 || pgid == std::process::id() {
        return false;
    }
    // SAFETY: signal zero probes an owned group without delivering a signal.
    let result = unsafe { libc::kill(-(pgid as i32), 0) };
    let error = std::io::Error::last_os_error();
    #[cfg(test)]
    if result != 0 && error.raw_os_error() != Some(libc::ESRCH) {
        eprintln!("probe {pgid}: {error}");
    }
    result != 0 && error.raw_os_error() == Some(libc::ESRCH)
}

/// Live (non-zombie) members of our process group, via /proc, or `None`
/// when the scan itself could not be trusted. `Some(0)` is a group VERIFIED
/// empty only after a kernel absence probe; `None` must NEVER be read as quiet (review
/// round-4 F5). Used to make the shutdown ladder honest about descendants:
/// a leader that exits while its group keeps running is not a terminated
/// shutdown, and a normal leader exit is not a cleaned-up group either.
#[cfg(target_os = "linux")]
fn group_live_scan(pgid: u32) -> Option<usize> {
    group_live_scan_at(pgid, std::path::Path::new("/proc"))
}

#[cfg(target_os = "linux")]
fn group_live_scan_at(pgid: u32, proc_root: &std::path::Path) -> Option<usize> {
    if kernel_group_absent(pgid) {
        return Some(0);
    }
    let Ok(entries) = std::fs::read_dir(proc_root) else {
        return None;
    };
    // A pid that vanished between `readdir` and the read is normal; a pid
    // still present whose `stat` we cannot read is not, and must not be
    // silently counted as "not a member".
    let present = |pid: u32| proc_root.join(pid.to_string()).exists();
    let mut live = 0;
    for entry in entries {
        let Ok(entry) = entry else { return None };
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        // Never count our own supervisor process even on a mislabelled
        // group; only the reaper's children should ever share a pgid.
        if pid == std::process::id() {
            continue;
        }
        let Ok(stat) = std::fs::read_to_string(proc_root.join(pid.to_string()).join("stat")) else {
            if present(pid) {
                return None;
            }
            continue;
        };
        let Some(close) = stat.rfind(')') else {
            if present(pid) {
                return None;
            }
            continue;
        };
        // After `comm`: state, ppid, pgrp, session, …
        let mut fields = stat[close + 1..].split_whitespace();
        let (Some(state), Some(_ppid), Some(pgrp)) = (fields.next(), fields.next(), fields.next())
        else {
            if present(pid) {
                return None;
            }
            continue;
        };
        match pgrp.parse::<u32>() {
            Ok(pgrp) if pgrp == pgid && state != "Z" => live += 1,
            Ok(_) => {}
            Err(_) if present(pid) => return None,
            Err(_) => {}
        }
    }
    // A different PID namespace (or restricted proc mount) may hide all
    // members. Only the kernel can corroborate absence. Zombie-only groups
    // conservatively remain uncertain until their parent reaps them.
    if live == 0 && !kernel_group_absent(pgid) {
        return None;
    }
    Some(live)
}

#[cfg(not(target_os = "linux"))]
fn group_live_scan(pgid: u32) -> Option<usize> {
    kernel_group_absent(pgid).then_some(0)
}

/// Whether process-group liveness is observable at all. Non-Linux
/// cleanup must never be read as certainty (AGENTKIT-110).
#[cfg(target_os = "linux")]
fn group_liveness_observable() -> bool {
    true
}

#[cfg(not(target_os = "linux"))]
fn group_liveness_observable() -> bool {
    true
}

/// Build a successful ladder receipt: on platforms where group liveness is
/// unobservable the receipt stays honest with `cleanup_uncertain` and a
/// detail noting descendants were never verified (AGENTKIT-110).
fn success_receipt(label: &str, sigterm: bool, sigkill: bool, note: &str) -> InterruptReceipt {
    let observable = group_liveness_observable();
    InterruptReceipt {
        terminated: true,
        escalated_to_sigterm: sigterm,
        escalated_to_sigkill: sigkill,
        cleanup_uncertain: !observable,
        detail: Some(if observable {
            format!("policy={label}; {note}")
        } else {
            format!("policy={label}; {note}; process-group descendants unverified")
        }),
    }
}

/// One ladder rung: the leader must be reaped and the whole group must go
/// quiet within `ms`.
async fn await_rung(shared: &Arc<SharedChild>, ms: i64) -> bool {
    let budget = Duration::from_millis(ms.max(1) as u64);
    tokio::time::timeout(budget, async {
        loop {
            if shared.exit_now().is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        if !group_liveness_observable() {
            return true;
        }
        loop {
            match group_live_scan(shared.pgid) {
                Some(0) => return true,
                Some(_) => tokio::time::sleep(Duration::from_millis(10)).await,
                None => return false,
            }
        }
    })
    .await
    .unwrap_or(false)
}

/// Receipt for a cleanup whose CONFIRMATION failed: the leader is gone,
/// but the group could not be proven empty (review round-4 F5).
fn uncertain_receipt(
    label: &str,
    sigterm: bool,
    sigkill: bool,
    note: impl Into<String>,
) -> InterruptReceipt {
    InterruptReceipt {
        terminated: true,
        escalated_to_sigterm: sigterm,
        escalated_to_sigkill: sigkill,
        cleanup_uncertain: true,
        detail: Some(format!("policy={label}; {}", note.into())),
    }
}

/// Bounded wait for the owned group to be VERIFIED empty. A failed scan is
/// uncertainty, never quiet (review round-4 F5).
async fn await_group_quiet(shared: &Arc<SharedChild>, ms: i64) -> bool {
    let budget = Duration::from_millis(ms.max(1) as u64);
    tokio::time::timeout(budget, async {
        loop {
            match group_live_scan(shared.pgid) {
                Some(0) => return true,
                Some(_) => tokio::time::sleep(Duration::from_millis(10)).await,
                None => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
    })
    .await
    .unwrap_or(false)
}

/// Review round-4 F5: reconcile the owned process group after the LEADER
/// exited on the normal `wait` path (whether it exited on its own or after
/// an acknowledged interrupt — the exit facts themselves are never
/// touched). A leader that emitted its root result and exited 0
/// while a same-group descendant keeps writing is NOT a finished attempt:
/// the descendant is still ours and must not survive the attempt. Called
/// by `wait` BEFORE finalization (which disarms drop cleanup and
/// deregisters), reusing the ladder's own machinery: SIGTERM the group,
/// await the operator grace, SIGKILL stragglers, then VERIFY empty.
///
/// The leader's exit facts are never touched — only group ownership is
/// settled. What cannot be proven is reported, not assumed: an unobservable
/// platform, a failed /proc scan, or a member that survives SIGKILL (e.g.
/// stuck in D state) all come back `cleanup_uncertain` with an honest
/// detail.
async fn reconcile_group_after_leader_exit(
    shared: &Arc<SharedChild>,
    limits: &ResourceLimits,
    after_interrupt: bool,
) -> InterruptReceipt {
    let label = if after_interrupt {
        "interruptedCompletion"
    } else {
        "normalCompletion"
    };
    let cause = if after_interrupt {
        "leader exited after the interrupt"
    } else {
        "leader exited normally"
    };
    if !group_liveness_observable() {
        // Nothing observable: best-effort sweep like the ladder's
        // unobservable-platform branch, and stay honest about it.
        let _ = group_kill(shared.pgid, SIGKILL);
        return success_receipt(
            label,
            false,
            true,
            &format!("{cause}; process-group descendants unverifiable on this platform"),
        );
    }
    let Some(stragglers) = group_live_scan(shared.pgid) else {
        let _ = group_kill(shared.pgid, SIGKILL);
        if await_group_quiet(shared, limits.cleanup_timeout_ms).await {
            return success_receipt(label, false, true, "process group absent after SIGKILL");
        }
        return uncertain_receipt(
            label,
            false,
            true,
            format!("{cause} but the process-group scan failed: descendants unverified"),
        );
    };
    if stragglers == 0 {
        return success_receipt(
            label,
            false,
            false,
            &format!("{cause}; process group already empty"),
        );
    }
    if let Err(reason) = group_kill(shared.pgid, SIGTERM) {
        return uncertain_receipt(
            label,
            false,
            false,
            format!("{cause} with {stragglers} descendant(s) and {reason}"),
        );
    }
    if await_group_quiet(shared, limits.terminate_grace_ms).await {
        return success_receipt(
            label,
            true,
            false,
            &format!("{cause}; {stragglers} descendant(s) exited after SIGTERM"),
        );
    }
    if let Err(reason) = group_kill(shared.pgid, SIGKILL) {
        return uncertain_receipt(
            label,
            true,
            false,
            format!("SIGTERM left descendants and {reason}"),
        );
    }
    if await_group_quiet(shared, limits.cleanup_timeout_ms).await {
        return success_receipt(
            label,
            true,
            true,
            &format!("{cause}; {stragglers} descendant(s) exited after SIGKILL"),
        );
    }
    let remaining = group_live_scan(shared.pgid).unwrap_or(usize::MAX);
    uncertain_receipt(
        label,
        true,
        true,
        format!(
            "{cause} but {remaining} process-group member(s) survived SIGKILL \
             (uninterruptible or unobservable): descendants unverified"
        ),
    )
}

/// Core bounded shutdown ladder shared by `handle.interrupt` and
/// `drain_owned`: SIGINT the group, await grace; SIGTERM, await grace;
/// SIGKILL, await cleanup budget. Idempotent via the cached receipt.
async fn interrupt_shared(
    shared: &Arc<SharedChild>,
    policy: InterruptPolicy,
    limits: &ResourceLimits,
) -> InterruptReceipt {
    if let Some(cached) = shared.cached() {
        return cached;
    }
    shared.interrupt_requested.store(true, Ordering::SeqCst);
    *shared.interrupt_policy.lock().unwrap() = Some(policy_label(policy).to_owned());

    let label = policy_label(policy);
    if let Err(reason) = group_kill(shared.pgid, SIGINT) {
        if reason.starts_with("refusing") {
            let receipt = InterruptReceipt {
                terminated: false,
                escalated_to_sigterm: false,
                escalated_to_sigkill: false,
                cleanup_uncertain: true,
                detail: Some(format!("policy={label}; {reason}")),
            };
            shared.store_receipt(&receipt);
            return receipt;
        }
        // ESRCH is mapped to Ok above; other errors keep escalating but
        // remain recorded as uncertainty.
        let receipt = InterruptReceipt {
            terminated: shared.exit_now().is_some(),
            escalated_to_sigterm: false,
            escalated_to_sigkill: false,
            cleanup_uncertain: true,
            detail: Some(format!("policy={label}; {reason}")),
        };
        shared.store_receipt(&receipt);
        return receipt;
    }

    if await_rung(shared, limits.interrupt_grace_ms).await {
        // Leader reaped: where group liveness is unobservable, sweep the
        // group one last time best-effort and stay honest in the receipt.
        if !group_liveness_observable() {
            let _ = group_kill(shared.pgid, SIGKILL);
        }
        let receipt = success_receipt(label, false, false, "exited after SIGINT");
        shared.store_receipt(&receipt);
        return receipt;
    }

    let _ = group_kill(shared.pgid, SIGTERM);
    if await_rung(shared, limits.terminate_grace_ms).await {
        if !group_liveness_observable() {
            // Leader reaped: final best-effort group sweep (C6).
            let _ = group_kill(shared.pgid, SIGKILL);
        }
        let receipt = success_receipt(label, true, false, "exited after SIGTERM");
        shared.store_receipt(&receipt);
        return receipt;
    }

    let _ = group_kill(shared.pgid, SIGKILL);
    if await_rung(shared, limits.cleanup_timeout_ms).await {
        // The group SIGKILL above already swept unobservable descendants.
        let receipt = success_receipt(label, true, true, "exited after SIGKILL");
        shared.store_receipt(&receipt);
        return receipt;
    }

    InterruptReceipt {
        terminated: false,
        escalated_to_sigterm: true,
        escalated_to_sigkill: true,
        cleanup_uncertain: true,
        detail: Some(format!(
            "policy={label}; child unreaped after SIGKILL within {}ms",
            limits.cleanup_timeout_ms
        )),
    }
}

fn deregister(live: &Arc<Mutex<HashMap<u32, std::sync::Weak<SharedChild>>>>, s: &Arc<SharedChild>) {
    let mut map = live.lock().unwrap();
    if let Some(weak) = map.get(&s.pid)
        && weak.as_ptr() == Arc::as_ptr(s)
    {
        map.remove(&s.pid);
    }
}

/// Bounded history of normal-completion descendant outcomes (review
/// round-4 F5): the newest attempts only, so a long-lived supervisor never
/// accumulates records without limit.
const MAX_CLEANUP_REPORTS: usize = 64;

fn record_cleanup_report(
    reports: &Arc<Mutex<HashMap<u32, (u64, InterruptReceipt)>>>,
    shared: &Arc<SharedChild>,
    receipt: &InterruptReceipt,
) {
    let mut map = reports.lock().unwrap();
    if map.len() >= MAX_CLEANUP_REPORTS {
        let floor = shared.generation.saturating_sub(MAX_CLEANUP_REPORTS as u64);
        map.retain(|_, (generation, _)| *generation > floor);
    }
    map.insert(shared.pid, (shared.generation, receipt.clone()));
}

/// Framed stdout reader: incremental newline framing with a hard
/// per-frame byte bound; the bound is checked BEFORE every extension and
/// BEFORE sending a completed line, so an oversized frame can never be
/// assembled or delivered. Oversized frames produce a single
/// `frame_overflow` fault and stop reading (AGENTKIT-110). A frame of
/// exactly `max_frame_bytes` (newline excluded) passes.
fn spawn_stdout_reader(
    mut stdout: tokio::process::ChildStdout,
    tx: mpsc::Sender<FrameLine>,
    max_frame_bytes: usize,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let overflow = || {
            FrameLine::Fault(fault(
                "frame_overflow",
                format!("stdout frame exceeded {max_frame_bytes} bytes"),
            ))
        };
        let mut chunk = vec![0u8; 16 * 1024];
        let mut line: Vec<u8> = Vec::new();
        loop {
            let n = match stdout.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            let mut rest = &chunk[..n];
            while let Some(pos) = rest.iter().position(|&b| b == b'\n') {
                // The completed frame is `line` plus `pos` bytes: reject
                // before it is ever built or sent.
                if line.len() + pos > max_frame_bytes {
                    let _ = tx.send(overflow()).await;
                    return;
                }
                line.extend_from_slice(&rest[..pos]);
                rest = &rest[pos + 1..];
                let framed = std::mem::take(&mut line);
                if tx.send(FrameLine::Line(framed)).await.is_err() {
                    return;
                }
            }
            // Un-terminated residue: bound checked before the extension,
            // never after it.
            if line.len() + rest.len() > max_frame_bytes {
                let _ = tx.send(overflow()).await;
                return;
            }
            line.extend_from_slice(rest);
        }
        if !line.is_empty() {
            let _ = tx.send(FrameLine::Line(line)).await;
        }
    })
}

/// Bounded stderr tail: a ring of the most recent `max_stderr_bytes`.
fn spawn_stderr_reader(
    mut stderr: tokio::process::ChildStderr,
    tail: Arc<Mutex<VecDeque<u8>>>,
    max_stderr_bytes: usize,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut buf = vec![0u8; 8 * 1024];
        loop {
            let n = match stderr.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            let mut guard = tail.lock().unwrap();
            guard.extend(&buf[..n]);
            let excess = guard.len().saturating_sub(max_stderr_bytes);
            if excess > 0 {
                guard.drain(..excess);
            }
        }
    })
}

/// The owned child handle (AGENTKIT-110). Carries its generation so a
/// stale handle can never be mistaken for a live authority, and the
/// reader/stdin task handles so `wait` leaves no task leaks.
///
/// Stream EOF, observed process exit and completed cleanup are THREE
/// separate facts: stdout EOF alone never disarms drop cleanup (AGENTKIT-
/// 110 lifecycle hardening — a child that closed stdout and kept running
/// must still be terminated when its handle is abandoned).
struct OwnedChild {
    shared: Arc<SharedChild>,
    limits: ResourceLimits,
    rx: mpsc::Receiver<FrameLine>,
    stderr_tail: Arc<Mutex<VecDeque<u8>>>,
    stdin: Arc<AsyncMutex<Option<ChildStdin>>>,
    written_stdin: usize,
    reader_tasks: Vec<JoinHandle<()>>,
    stdin_tasks: Vec<CancellationToken>,
    /// The stdout frame channel closed; no further frames will arrive.
    /// NOT a claim about process exit.
    frames_exhausted: bool,
    /// Cleanup ran to completion through `wait`/`interrupt`/drop: tasks
    /// are stopped and the child is deregistered. Only this disarms Drop.
    /// `wait` may only set it AFTER the round-4 F5 group reconcile — a
    /// leader exit alone never proves the group is gone.
    finalized: bool,
    live: Arc<Mutex<HashMap<u32, std::sync::Weak<SharedChild>>>>,
    /// Review round-4 F5: where the normal-completion descendant outcome is
    /// recorded (read back via
    /// [`OwnedProcessSupervisor::descendant_cleanup_report`] / the
    /// [`ProcessSupervisor::descendant_cleanup`] port method).
    cleanup_reports: Arc<Mutex<HashMap<u32, (u64, InterruptReceipt)>>>,
}

impl OwnedChild {
    /// Stop the tasks and retire the registry entry. Callers must have
    /// settled the process GROUP first (review round-4 F5): `wait` runs
    /// [`reconcile_group_after_leader_exit`] before this, and the abandon
    /// path sweeps the group in `Drop`.
    fn finalize(&mut self) {
        self.frames_exhausted = true;
        self.finalized = true;
        for t in self.reader_tasks.drain(..) {
            t.abort();
        }
        for t in self.stdin_tasks.drain(..) {
            t.cancel();
        }
        deregister(&self.live, &self.shared);
    }

    async fn await_process_exit(&self) -> RawExit {
        loop {
            if let Some(raw) = self.shared.exit_now() {
                return raw;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }
}

impl Drop for OwnedChild {
    /// An abandoned (never finalized) handle must not orphan its child:
    /// the reader/stdin tasks are aborted here, and a detached bounded
    /// cleanup task drives the same SIGINT→SIGTERM→SIGKILL ladder the
    /// supervisor owns, then deregisters (AGENTKIT-110). `kill_on_drop(false)`
    /// keeps the reaper task as the single waiter, so the dropped process is
    /// still reaped once the ladder terminates it. Without a runtime in
    /// context the strongest honest fallback is an immediate group SIGKILL.
    ///
    /// Stdout EOF does NOT excuse cleanup: only `finalized` (cleanup
    /// already completed) or a reaper-confirmed exit skips the ladder.
    fn drop(&mut self) {
        if self.finalized {
            return;
        }
        self.finalized = true;
        for t in self.reader_tasks.drain(..) {
            t.abort();
        }
        for t in self.stdin_tasks.drain(..) {
            t.cancel();
        }
        // Exit already observed by the reaper (EOF-then-exit, handle
        // dropped without `wait`): the LEADER is gone, but a same-group
        // tool descendant may survive — abandon means no survivors, so
        // best-effort sweep the group like the success receipt does,
        // then deregister (review round-3 F2).
        if self.shared.exit_now().is_some() {
            let _ = group_kill(self.shared.pgid, SIGKILL);
            deregister(&self.live, &self.shared);
            return;
        }
        let shared = Arc::clone(&self.shared);
        let limits = self.limits.clone();
        let live = Arc::clone(&self.live);
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    interrupt_shared(&shared, InterruptPolicy::HostShutdown, &limits).await;
                    deregister(&live, &shared);
                });
            }
            Err(_) => {
                let _ = group_kill(shared.pgid, SIGKILL);
                deregister(&live, &shared);
            }
        }
    }
}

#[async_trait]
impl OwnedChildHandle for OwnedChild {
    fn pid(&self) -> u32 {
        self.shared.pid
    }

    fn generation(&self) -> u64 {
        self.shared.generation
    }

    async fn next_frame(&mut self) -> Option<FrameLine> {
        if self.finalized || self.frames_exhausted {
            return None;
        }
        match self.rx.recv().await {
            Some(frame) => Some(frame),
            None => {
                // Channel closed: no more frames. The process may still be
                // running — drop cleanup stays armed until `wait` or the
                // reaper-confirmed exit retires it.
                self.frames_exhausted = true;
                None
            }
        }
    }

    fn stderr_tail(&self) -> Vec<u8> {
        self.stderr_tail.lock().unwrap().iter().copied().collect()
    }

    async fn deliver_input(
        &mut self,
        bytes: &[u8],
        close_stdin: bool,
    ) -> Result<(), TransportFault> {
        if self
            .written_stdin
            .checked_add(bytes.len())
            .is_none_or(|total| total > self.limits.max_stdin_bytes)
        {
            return Err(fault(
                "stdin_overflow",
                format!(
                    "stdin delivery would exceed {} bytes",
                    self.limits.max_stdin_bytes
                ),
            ));
        }
        // Write on a dedicated task: a blocked pipe must never starve the
        // (already independent) stdout/stderr readers or hang the caller
        // past the cleanup budget. The task is cancellable so finalize
        // never leaves it parked on a dead pipe.
        let stdin = Arc::clone(&self.stdin);
        let data = bytes.to_vec();
        let cancel = CancellationToken::new();
        let write_cancel = cancel.clone();
        let task = tokio::spawn(async move {
            let writer_task = async move {
                let mut guard = stdin.lock().await;
                let Some(writer) = guard.as_mut() else {
                    return Err(fault("stdin_broken", "stdin already closed"));
                };
                if !data.is_empty()
                    && let Err(err) = writer.write_all(&data).await
                {
                    return Err(fault("stdin_broken", format!("stdin write failed: {err}")));
                }
                if let Err(err) = writer.flush().await {
                    return Err(fault("stdin_broken", format!("stdin flush failed: {err}")));
                }
                if close_stdin {
                    if let Err(err) = writer.shutdown().await {
                        return Err(fault(
                            "stdin_broken",
                            format!("stdin shutdown failed: {err}"),
                        ));
                    }
                    *guard = None;
                }
                Ok(data.len())
            };
            tokio::select! {
                () = write_cancel.cancelled() => {
                    Err(fault("stdin_broken", "stdin write cancelled"))
                }
                outcome = writer_task => outcome,
            }
        });
        self.stdin_tasks.push(cancel);
        let budget = Duration::from_millis(self.limits.cleanup_timeout_ms.max(1) as u64);
        match tokio::time::timeout(budget, task).await {
            Ok(Ok(Ok(n))) => {
                self.written_stdin += n;
                Ok(())
            }
            Ok(Ok(Err(f))) => Err(f),
            Ok(Err(_)) => Err(fault("stdin_broken", "stdin writer task lost")),
            Err(_) => Err(fault(
                "stdin_blocked",
                "stdin write stalled past the cleanup budget",
            )),
        }
    }

    async fn interrupt(&mut self, policy: InterruptPolicy) -> InterruptReceipt {
        let receipt = interrupt_shared(&self.shared, policy, &self.limits).await;
        self.finalize();
        receipt
    }

    async fn wait(&mut self) -> ProcessExit {
        let raw = self.await_process_exit().await;
        // Review round-4 F5: the LEADER exiting is not the GROUP exiting.
        // A leader that printed a valid successful root result and exited 0
        // can leave a same-group descendant running, and `finalize` below
        // both disarms drop cleanup and deregisters the attempt — so the
        // owned group is reconciled (SIGTERM → grace → SIGKILL → verify
        // empty, the ladder's own machinery) BEFORE anything is disarmed.
        // The leader's exit facts are recorded first and stay unchanged.
        let after_interrupt = self.shared.interrupt_requested.load(Ordering::SeqCst);
        let receipt =
            reconcile_group_after_leader_exit(&self.shared, &self.limits, after_interrupt).await;
        record_cleanup_report(&self.cleanup_reports, &self.shared, &receipt);
        self.finalize();
        ProcessExit {
            pid: self.shared.pid,
            generation: self.shared.generation,
            exit_code: raw.exit_code,
            signal: raw.signal,
            after_interrupt: self.shared.interrupt_requested.load(Ordering::SeqCst),
        }
    }
}

#[async_trait]
impl ProcessSupervisor for OwnedProcessSupervisor {
    async fn spawn(
        &self,
        plan: &LaunchPlan,
        limits: &ResourceLimits,
        cancellation: &CancellationToken,
    ) -> Result<Box<dyn OwnedChildHandle>, NativeError> {
        let Some(executable) = plan.argv.first() else {
            return Err(NativeError::Transport {
                code: "empty_argv".to_owned(),
                message: "launch plan carries no argv[0] executable".to_owned(),
            });
        };
        if cancellation.is_cancelled() {
            return Err(NativeError::Cancelled);
        }

        let mut command = tokio::process::Command::new(executable);
        command
            .args(&plan.argv[1..])
            .current_dir(&plan.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(false);
        // Strip ambient credential families BEFORE the pinned additive
        // environment is applied, so the vendor CLI sees only the profile
        // this delivery owns.
        for key in credential_env_keys() {
            command.env_remove(key);
        }
        for (key, value) in &plan.env {
            command.env(key, value);
        }
        // Ambient interrupted-turn resume is disabled for this scope, even
        // if the launch plan or ambient environment tries to set it.
        command.env_remove(INTERRUPTED_TURN_RESUME_ENV);
        #[cfg(unix)]
        {
            command.process_group(0);
        }

        let mut child = command.spawn().map_err(|err| NativeError::Transport {
            code: "spawn_failed".to_owned(),
            message: format!("failed to spawn {}: {err}", executable),
        })?;
        let pid = child.id().ok_or_else(|| NativeError::Transport {
            code: "spawn_failed".to_owned(),
            message: "spawned child has no pid".to_owned(),
        })?;
        let generation = self.next_generation.fetch_add(1, Ordering::SeqCst) + 1;

        let shared = Arc::new(SharedChild {
            pid,
            pgid: pid,
            generation,
            limits: limits.clone(),
            exit: Mutex::new(None),
            interrupt_requested: AtomicBool::new(false),
            interrupt_policy: Mutex::new(None),
            cached_receipt: Mutex::new(None),
        });
        self.live
            .lock()
            .unwrap()
            .insert(pid, Arc::downgrade(&shared));

        let stdin = child.stdin.take().ok_or_else(|| NativeError::Transport {
            code: "spawn_failed".to_owned(),
            message: "spawned child has no stdin pipe".to_owned(),
        })?;
        let stdout = child.stdout.take().ok_or_else(|| NativeError::Transport {
            code: "spawn_failed".to_owned(),
            message: "spawned child has no stdout pipe".to_owned(),
        })?;
        let stderr = child.stderr.take().ok_or_else(|| NativeError::Transport {
            code: "spawn_failed".to_owned(),
            message: "spawned child has no stderr pipe".to_owned(),
        })?;

        // Reaper: the single awaiter of the child; publishes exit facts for
        // wait(), the interrupt ladder and drain alike.
        let reaper_shared = Arc::clone(&shared);
        tokio::spawn(async move {
            let status = child.wait().await;
            let raw = match status {
                Ok(status) => RawExit {
                    exit_code: status.code(),
                    signal: status.signal(),
                },
                // Wait failed: record the uncertainty as a signal-less
                // unknown exit so waiters resolve rather than hang.
                Err(_) => RawExit {
                    exit_code: None,
                    signal: None,
                },
            };
            *reaper_shared.exit.lock().unwrap() = Some(raw);
        });

        // Readers start immediately, independent of any input delivery.
        let (tx, rx) = mpsc::channel(limits.max_pending_events.max(1));
        let stderr_tail = Arc::new(Mutex::new(VecDeque::new()));
        let stdout_task = spawn_stdout_reader(stdout, tx, limits.max_frame_bytes);
        let stderr_task =
            spawn_stderr_reader(stderr, Arc::clone(&stderr_tail), limits.max_stderr_bytes);

        Ok(Box::new(OwnedChild {
            shared,
            limits: limits.clone(),
            rx,
            stderr_tail,
            stdin: Arc::new(AsyncMutex::new(Some(stdin))),
            written_stdin: 0,
            reader_tasks: vec![stdout_task, stderr_task],
            stdin_tasks: Vec::new(),
            frames_exhausted: false,
            finalized: false,
            live: Arc::clone(&self.live),
            cleanup_reports: Arc::clone(&self.cleanup_reports),
        }))
    }

    async fn drain_owned(&self, policy: InterruptPolicy) -> Vec<InterruptReceipt> {
        let live: Vec<Arc<SharedChild>> = {
            let mut map = self.live.lock().unwrap();
            let live = map.values().filter_map(std::sync::Weak::upgrade).collect();
            map.clear();
            live
        };
        let mut receipts = Vec::with_capacity(live.len());
        for shared in live {
            receipts.push(interrupt_shared(&shared, policy, &shared.limits).await);
        }
        receipts
    }

    /// Per-attempt scoped cleanup: terminate exactly the live child whose
    /// registry identity is `(pid, generation)` and nothing else. A stale
    /// generation (pid reused after exit) matches nothing, so a late
    /// cleanup can never kill a newer sibling process.
    async fn terminate_owned(
        &self,
        pid: u32,
        generation: u64,
        policy: InterruptPolicy,
    ) -> Option<InterruptReceipt> {
        let shared = {
            let map = self.live.lock().unwrap();
            map.get(&pid)
                .and_then(std::sync::Weak::upgrade)
                .filter(|s| s.generation == generation)
        }?;
        let receipt = interrupt_shared(&shared, policy, &shared.limits).await;
        let mut map = self.live.lock().unwrap();
        if let Some(weak) = map.get(&pid)
            && weak.as_ptr() == Arc::as_ptr(&shared)
        {
            map.remove(&pid);
        }
        Some(receipt)
    }

    /// Review round-4 F5: the portable read-back of the normal-completion
    /// group reconcile recorded by `wait`, delegating to the concrete
    /// ledger accessor so both access paths report identically.
    fn descendant_cleanup(&self, pid: u32, generation: u64) -> Option<InterruptReceipt> {
        self.descendant_cleanup_report(pid, generation)
    }
}

/// The whole-runtime shutdown owner (review round-3 F1, AGENTKIT-110):
/// once the last reference to the supervisor is gone, every still-live
/// registered vendor process is an orphan of a retired runtime and must
/// not outlive it. Synchronous group SIGKILL is honest best-effort here:
/// no runtime is required, and any surviving reaper task still reaps.
impl Drop for OwnedProcessSupervisor {
    fn drop(&mut self) {
        let live: Vec<Arc<SharedChild>> = {
            let mut map = self.live.lock().unwrap();
            let live = map.values().filter_map(std::sync::Weak::upgrade).collect();
            map.clear();
            live
        };
        for shared in live {
            let _ = group_kill(shared.pgid, SIGKILL);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentkit_native::PermissionPromptPolicy;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use tempfile::TempDir;

    #[cfg(target_os = "linux")]
    #[test]
    fn empty_or_missing_proc_never_proves_live_group_absent() {
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: getpgrp has no arguments and only reads process identity.
        let group = unsafe { libc::getpgrp() } as u32;
        assert!(!kernel_group_absent(group));
        assert_eq!(group_live_scan_at(group, dir.path()), None);
        assert_eq!(group_live_scan_at(group, &dir.path().join("missing")), None);
    }

    fn fixture(dir: &TempDir, name: &str, script: &str) -> PathBuf {
        let path = dir.path().join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn plan_for(exe: PathBuf, dir: &TempDir) -> LaunchPlan {
        LaunchPlan {
            argv: vec![exe.to_string_lossy().into_owned()],
            env: vec![],
            cwd: dir.path().to_path_buf(),
            per_run_dir: dir.path().to_path_buf(),
            permission_policy: PermissionPromptPolicy::None,
            max_vendor_turns: None,
            native_session_id: None,
            fingerprint: "test".to_owned(),
        }
    }

    fn tight_limits() -> ResourceLimits {
        ResourceLimits {
            max_frame_bytes: 64 * 1024,
            max_stderr_bytes: 4096,
            max_pending_events: 64,
            max_stdin_bytes: 1024 * 1024,
            startup_timeout_ms: 5_000,
            idle_timeout_ms: 5_000,
            interrupt_grace_ms: 2_000,
            terminate_grace_ms: 2_000,
            cleanup_timeout_ms: 3_000,
        }
    }

    async fn spawn_child(
        sup: &OwnedProcessSupervisor,
        exe: PathBuf,
        dir: &TempDir,
        limits: &ResourceLimits,
    ) -> Box<dyn OwnedChildHandle> {
        let plan = plan_for(exe, dir);
        let token = CancellationToken::new();
        // The fixture file was just written; the kernel may still report
        // ETXTBSY for a moment after close, so retry briefly.
        let mut attempts = 0;
        loop {
            match sup.spawn(&plan, limits, &token).await {
                Ok(child) => return child,
                Err(NativeError::Transport {
                    ref code,
                    ref message,
                }) if code == "spawn_failed"
                    && message.contains("Text file busy")
                    && attempts < 25 =>
                {
                    attempts += 1;
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(other) => panic!("spawn: {other:?}"),
            }
        }
    }

    async fn read_frame(child: &mut dyn OwnedChildHandle, ms: u64) -> Option<FrameLine> {
        tokio::time::timeout(Duration::from_millis(ms), child.next_frame())
            .await
            .expect("frame within budget")
    }

    fn line(bytes: &[u8]) -> FrameLine {
        FrameLine::Line(bytes.to_vec())
    }

    /// AGENTKIT-110: a child that emits nothing until it receives its
    /// prompt still streams — readers start at spawn, delivery is
    /// independent, and no prompt-after-stream deadlock is possible.
    #[tokio::test]
    async fn prompt_waiting_fake_child_completes() {
        let dir = TempDir::new().unwrap();
        let exe = fixture(
            &dir,
            "prompt_child.sh",
            "IFS= read -r line\nprintf '{\"n\":1}\\n{\"n\":2}\\n'",
        );
        let sup = OwnedProcessSupervisor::new();
        let mut child = spawn_child(&sup, exe, &dir, &tight_limits()).await;

        // Nothing arrives before the prompt is delivered.
        assert!(
            tokio::time::timeout(Duration::from_millis(250), child.next_frame())
                .await
                .is_err()
        );

        child.deliver_input(b"hello prompt\n", true).await.unwrap();
        assert_eq!(
            read_frame(child.as_mut(), 5_000).await,
            Some(line(b"{\"n\":1}"))
        );
        assert_eq!(
            read_frame(child.as_mut(), 5_000).await,
            Some(line(b"{\"n\":2}"))
        );
        assert_eq!(child.next_frame().await, None);

        let exit = child.wait().await;
        assert_eq!(exit.exit_code, Some(0));
        assert_eq!(exit.signal, None);
        assert!(!exit.after_interrupt);
        assert_eq!(exit.pid, child.pid());
        assert_eq!(exit.generation, child.generation());
        // Stale fencing: after wait the stream stays closed.
        assert_eq!(child.next_frame().await, None);
        // wait() deregistered the child: drain sees nothing.
        assert!(
            sup.drain_owned(InterruptPolicy::HostShutdown)
                .await
                .is_empty()
        );
    }

    /// AGENTKIT-110: framing is byte-level; a UTF-8 sequence split across
    /// writes (and even across read boundaries) yields one correct frame.
    #[tokio::test]
    async fn split_utf8_and_newline_boundary() {
        let dir = TempDir::new().unwrap();
        // 'é' (U+00E9 = 0xC3 0xA9) emitted in two writes, newline last.
        let exe = fixture(&dir, "split_utf8.sh", "printf '\\303'\nprintf '\\251\\n'");
        let sup = OwnedProcessSupervisor::new();
        let mut child = spawn_child(&sup, exe, &dir, &tight_limits()).await;

        assert_eq!(
            read_frame(child.as_mut(), 5_000).await,
            Some(line(&[0xC3, 0xA9]))
        );
        assert_eq!(child.next_frame().await, None);
        let exit = child.wait().await;
        assert_eq!(exit.exit_code, Some(0));
    }

    /// AGENTKIT-110: an oversized single frame produces one
    /// `frame_overflow` fault and stops reading; no unbounded buffering.
    #[tokio::test]
    async fn oversized_frame_fails_bounds() {
        let dir = TempDir::new().unwrap();
        let exe = fixture(
            &dir,
            "huge.sh",
            "head -c 10485760 /dev/zero | tr '\\0' 'x'\nprintf '\\n'",
        );
        let sup = OwnedProcessSupervisor::new();
        let mut child = spawn_child(&sup, exe, &dir, &tight_limits()).await;

        match read_frame(child.as_mut(), 10_000).await {
            Some(FrameLine::Fault(f)) => assert_eq!(f.code, "frame_overflow"),
            other => panic!("expected frame_overflow fault, got {other:?}"),
        }
        assert_eq!(child.next_frame().await, None);
        let _ = child.wait().await;
    }

    /// AGENTKIT-110: 100k small lines flow through the bounded channel;
    /// the producer awaits when the consumer lags, nothing is dropped.
    #[tokio::test]
    async fn output_flood_backpressure() {
        let dir = TempDir::new().unwrap();
        let exe = fixture(&dir, "flood.sh", "seq 1 100000 | sed 's/^/{\"i\":/;s/$/}/'");
        let sup = OwnedProcessSupervisor::new();
        let mut child = spawn_child(&sup, exe, &dir, &tight_limits()).await;

        let mut count = 0usize;
        let mut last = Vec::new();
        while let Some(frame) = read_frame(child.as_mut(), 30_000).await {
            match frame {
                FrameLine::Line(bytes) => {
                    count += 1;
                    last = bytes;
                }
                FrameLine::Fault(f) => panic!("unexpected fault in flood: {:?}", f),
            }
        }
        assert_eq!(count, 100_000);
        assert_eq!(last, b"{\"i\":100000}".to_vec());
        let exit = child.wait().await;
        assert_eq!(exit.exit_code, Some(0));
    }

    /// AGENTKIT-110: writing to a child that has closed its stdin reports
    /// the stable `stdin_broken` fault, never a hang.
    #[tokio::test]
    async fn blocked_pipe_and_broken_stdin() {
        let dir = TempDir::new().unwrap();
        // fd 0 is closed before the ready frame is emitted, so any later
        // write must see EPIPE whether the child is alive or already gone.
        // The child stays alive briefly so the fault comes from a closed
        // descriptor, not from process teardown timing.
        let exe = fixture(
            &dir,
            "close_stdin.sh",
            "exec 0<&-\nprintf 'r\\n'\nsleep 0.3\n",
        );
        let sup = OwnedProcessSupervisor::new();
        let mut child = spawn_child(&sup, exe, &dir, &tight_limits()).await;

        assert_eq!(read_frame(child.as_mut(), 5_000).await, Some(line(b"r")));
        // The child closes fd 0 before emitting the frame, but under load
        // it can be caught in D state with the close's deferred pipe
        // release still pending; wait until it is actually sleeping in
        // `sleep 0.3` so the write deterministically sees a dead pipe.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if process_state(child.pid()).is_some_and(|state| state.starts_with('S')) {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "child never reached a stable sleeping state"
            );
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        let err = child.deliver_input(b"too late\n", true).await.unwrap_err();
        assert_eq!(err.code, "stdin_broken");
        let exit = child.wait().await;
        assert_eq!(exit.exit_code, Some(0));
    }

    /// AGENTKIT-110: early exit with no output is EOF on the stream, not
    /// completion; the exit facts still arrive via wait().
    #[tokio::test]
    async fn early_exit() {
        let dir = TempDir::new().unwrap();
        let exe = fixture(&dir, "exit3.sh", "exit 3");
        let sup = OwnedProcessSupervisor::new();
        let mut child = spawn_child(&sup, exe, &dir, &tight_limits()).await;

        assert_eq!(child.next_frame().await, None);
        let exit = child.wait().await;
        assert_eq!(exit.exit_code, Some(3));
        assert!(!exit.after_interrupt);
    }

    /// AGENTKIT-110: a host drain while a large stdin write is blocked
    /// against a child that never reads terminates the child within the
    /// grace windows; the delivery faults instead of hanging.
    #[tokio::test]
    async fn cancel_during_write() {
        let dir = TempDir::new().unwrap();
        let exe = fixture(
            &dir,
            "greedy.sh",
            "trap 'exit 130' INT\ntrap 'exit 143' TERM\nwhile :; do :; done",
        );
        let sup = OwnedProcessSupervisor::new();
        let mut child = spawn_child(&sup, exe.clone(), &dir, &tight_limits()).await;

        let big = vec![b'x'; 256 * 1024]; // larger than the 64KiB pipe buffer
        let (receipts, delivery) = tokio::join!(
            sup.drain_owned(InterruptPolicy::UserStop),
            child.deliver_input(&big, false)
        );
        assert_eq!(receipts.len(), 1);
        assert!(receipts[0].terminated, "{:?}", receipts[0]);
        let err = delivery.expect_err("blocked write must fault, not hang");
        assert!(
            err.code == "stdin_broken" || err.code == "stdin_blocked",
            "unexpected fault {:?}",
            err
        );
        let exit = child.wait().await;
        assert!(exit.after_interrupt);
    }

    /// AGENTKIT-110: a child that honors SIGINT terminates the ladder at
    /// the first rung: no SIGTERM/SIGKILL escalation, after_interrupt set,
    /// and a second interrupt returns the cached receipt.
    #[tokio::test]
    async fn interrupt_then_exit() {
        let dir = TempDir::new().unwrap();
        let exe = fixture(
            &dir,
            "sigint_exit.sh",
            "sleep 30 & p=$!\ntrap 'kill $p 2>/dev/null; wait $p 2>/dev/null; exit 0' INT\nprintf 'r\\n'\nwait",
        );
        let sup = OwnedProcessSupervisor::new();
        let mut child = spawn_child(&sup, exe, &dir, &tight_limits()).await;
        assert_eq!(read_frame(child.as_mut(), 5_000).await, Some(line(b"r")));

        let receipt = child.interrupt(InterruptPolicy::UserStop).await;
        assert!(receipt.terminated, "{receipt:?}");
        assert!(!receipt.escalated_to_sigterm);
        assert!(!receipt.escalated_to_sigkill);
        assert!(!receipt.cleanup_uncertain);

        // Idempotent: the ladder result is cached, not replayed.
        let again = child.interrupt(InterruptPolicy::UserStop).await;
        assert_eq!(receipt, again);

        let exit = child.wait().await;
        assert!(exit.after_interrupt);
        assert_eq!(exit.exit_code, Some(0));
        assert_eq!(exit.signal, None);
    }

    /// AGENTKIT-110: shutdown reaches the whole process group, including
    /// background descendants; the group is verifiably dead afterwards.
    #[tokio::test]
    async fn supervisor_drop_kills_descendants() {
        let dir = TempDir::new().unwrap();
        // Deliberately a bare `sleep 60 &`: POSIX sh starts background
        // jobs with SIGINT ignored, so this descendant survives the first
        // rung and forces the ladder to escalate until the whole group is
        // dead — leader exit alone must never count as shutdown.
        // The descendant emits its own readiness frame, so the interrupt
        // cannot race its launch.
        let exe = fixture(
            &dir,
            "descendants.sh",
            "trap ':' INT\ntrap 'kill $p 2>/dev/null; wait $p 2>/dev/null; exit 0' TERM\nsh -c 'printf \"r\\n\"; exec sleep 60' & p=$!\nwhile kill -0 $p 2>/dev/null; do wait $p; done",
        );
        let sup = OwnedProcessSupervisor::new();
        let mut child = spawn_child(&sup, exe, &dir, &tight_limits()).await;
        assert_eq!(read_frame(child.as_mut(), 5_000).await, Some(line(b"r")));

        let pgid = child.pid();
        let receipt = child.interrupt(InterruptPolicy::UserStop).await;
        assert!(receipt.terminated, "{receipt:?}");
        assert!(
            receipt.escalated_to_sigterm,
            "stubborn background descendant must force escalation: {receipt:?}"
        );

        // Poll until the group holds no live member. An orphaned
        // descendant can linger as an unreaped ZOMBIE under a
        // non-reaping init: the signal has landed, so zombies count as
        // terminated (the receipt already proves the group leader was
        // reaped by the supervisor's own reaper).
        let group_has_live_member = |pgid: u32| -> bool {
            if !cfg!(target_os = "linux") {
                return !kernel_group_absent(pgid);
            }
            let Ok(entries) = std::fs::read_dir("/proc") else {
                return true;
            };
            for entry in entries.flatten() {
                let Some(pid) = entry
                    .file_name()
                    .to_str()
                    .and_then(|s| s.parse::<u32>().ok())
                else {
                    continue;
                };
                let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
                    continue;
                };
                let Some(close) = stat.rfind(')') else {
                    continue;
                };
                let fields: Vec<&str> = stat[close + 1..].split_whitespace().collect();
                // After `comm`: state, ppid, pgrp, …
                if fields.len() > 2 && fields[2].parse::<u32>() == Ok(pgid) && fields[0] != "Z" {
                    return true;
                }
            }
            false
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while group_has_live_member(pgid) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "process group {pgid} still alive after bounded shutdown"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let exit = child.wait().await;
        assert!(exit.after_interrupt);
    }

    /// AGENTKIT-110: drain_owned interrupts every live child exactly once,
    /// deregisters them (second drain is empty), and generations are
    /// strictly increasing per spawn.
    #[tokio::test]
    async fn drain_owned_stops_everything() {
        let dir = TempDir::new().unwrap();
        let live_script = "sleep 30 & p=$!\ntrap 'kill $p 2>/dev/null; wait $p 2>/dev/null; exit 0' INT\nprintf 'r\\n'\nwait";
        let sup = OwnedProcessSupervisor::new();
        let a = fixture(&dir, "live_a.sh", live_script);
        let b = fixture(&dir, "live_b.sh", live_script);
        let c = fixture(&dir, "quick.sh", "printf 'q\\n'");

        let mut child_a = spawn_child(&sup, a, &dir, &tight_limits()).await;
        assert_eq!(read_frame(child_a.as_mut(), 5_000).await, Some(line(b"r")));
        let gen_a = child_a.generation();

        let mut child_b = spawn_child(&sup, b, &dir, &tight_limits()).await;
        assert_eq!(read_frame(child_b.as_mut(), 5_000).await, Some(line(b"r")));
        assert!(child_b.generation() > gen_a);

        let mut child_c = spawn_child(&sup, c, &dir, &tight_limits()).await;
        assert_eq!(read_frame(child_c.as_mut(), 5_000).await, Some(line(b"q")));
        assert!(child_c.generation() > child_b.generation());
        // Reaping C deregisters it, so drain must not report it.
        let exit_c = child_c.wait().await;
        assert_eq!(exit_c.exit_code, Some(0));

        let receipts = sup.drain_owned(InterruptPolicy::HostShutdown).await;
        assert_eq!(receipts.len(), 2);
        for r in &receipts {
            assert!(r.terminated, "{r:?}");
            assert!(!r.cleanup_uncertain, "{r:?}");
        }
        assert!(
            sup.drain_owned(InterruptPolicy::HostShutdown)
                .await
                .is_empty()
        );

        let exit_a = child_a.wait().await;
        assert!(exit_a.after_interrupt);
        let exit_b = child_b.wait().await;
        assert!(exit_b.after_interrupt);
    }

    /// AGENTKIT-110/116: user stop and host shutdown remain distinct in
    /// the receipts and both mark their exits as interrupt-driven.
    #[tokio::test]
    async fn cancel_and_shutdown_reasons_are_distinct() {
        let dir = TempDir::new().unwrap();
        let script = "sleep 30 & p=$!\ntrap 'kill $p 2>/dev/null; wait $p 2>/dev/null; exit 0' INT\nprintf 'r\\n'\nwait";
        let sup = OwnedProcessSupervisor::new();
        let a = fixture(&dir, "reason_a.sh", script);
        let b = fixture(&dir, "reason_b.sh", script);

        let mut child_a = spawn_child(&sup, a, &dir, &tight_limits()).await;
        assert_eq!(read_frame(child_a.as_mut(), 5_000).await, Some(line(b"r")));
        let receipt_a = child_a.interrupt(InterruptPolicy::UserStop).await;
        assert!(receipt_a.terminated, "{receipt_a:?}");

        let mut child_b = spawn_child(&sup, b, &dir, &tight_limits()).await;
        assert_eq!(read_frame(child_b.as_mut(), 5_000).await, Some(line(b"r")));
        let receipt_b = child_b.interrupt(InterruptPolicy::HostShutdown).await;
        assert!(receipt_b.terminated, "{receipt_b:?}");

        let detail_a = receipt_a.detail.expect("user stop records its reason");
        let detail_b = receipt_b.detail.expect("host shutdown records its reason");
        assert!(detail_a.contains("userStop"), "{detail_a}");
        assert!(detail_b.contains("hostShutdown"), "{detail_b}");
        assert_ne!(detail_a, detail_b);

        assert!(child_a.wait().await.after_interrupt);
        assert!(child_b.wait().await.after_interrupt);
    }

    /// AGENTKIT-110 (C5): the frame bound is enforced BEFORE every
    /// extension and BEFORE sending a completed line. A 9-byte frame under
    /// an 8-byte cap must fault once with `frame_overflow` — including when
    /// the frame is assembled across two writes — while a frame of exactly
    /// the cap (newline excluded) passes.
    #[tokio::test]
    async fn frame_bound_enforced_before_send() {
        let dir = TempDir::new().unwrap();
        let mut limits = tight_limits();
        limits.max_frame_bytes = 8;

        // 9 bytes under an 8-byte cap → single frame_overflow fault.
        let exe = fixture(&dir, "nine.sh", "printf '123456789\\n'");
        let sup = OwnedProcessSupervisor::new();
        let mut child = spawn_child(&sup, exe, &dir, &limits).await;
        match read_frame(child.as_mut(), 5_000).await {
            Some(FrameLine::Fault(f)) => assert_eq!(f.code, "frame_overflow"),
            other => panic!("expected frame_overflow, got {other:?}"),
        }
        assert_eq!(child.next_frame().await, None);
        let _ = child.wait().await;

        // The same frame completed across two writes must fault instead of
        // ever sending a 9-byte line: `cat` flushes per stage, guaranteeing
        // the reader sees the 7-byte prefix before the completing "89\n".
        let exe = fixture(
            &dir,
            "nine_split.sh",
            "printf '1234567' | cat\nsleep 0.3\nprintf '89\\n' | cat",
        );
        let sup = OwnedProcessSupervisor::new();
        let mut child = spawn_child(&sup, exe, &dir, &limits).await;
        match read_frame(child.as_mut(), 5_000).await {
            Some(FrameLine::Fault(f)) => assert_eq!(f.code, "frame_overflow"),
            other => panic!("expected frame_overflow for split frame, got {other:?}"),
        }
        assert_eq!(child.next_frame().await, None);
        let _ = child.wait().await;

        // Exactly the cap passes (newline excluded from the count).
        let exe = fixture(&dir, "eight.sh", "printf '12345678\\n'");
        let sup = OwnedProcessSupervisor::new();
        let mut child = spawn_child(&sup, exe, &dir, &limits).await;
        assert_eq!(
            read_frame(child.as_mut(), 5_000).await,
            Some(line(b"12345678"))
        );
        assert_eq!(child.next_frame().await, None);
        let _ = child.wait().await;
    }

    /// AGENTKIT-110 (C6): dropping the owned handle — never `wait`ed or
    /// `interrupt`ed — and then the supervisor must leave no live process:
    /// the detached bounded cleanup ladder terminates the child and the
    /// supervisor's reaper (sole waiter, `kill_on_drop(false)`) reaps it.
    #[tokio::test]
    async fn dropped_handle_and_supervisor_reap_the_child() {
        let dir = TempDir::new().unwrap();
        let exe = fixture(&dir, "sleepy.sh", "printf 'r\\n'\nsleep 60");
        let sup = OwnedProcessSupervisor::new();
        let mut child = spawn_child(&sup, exe, &dir, &tight_limits()).await;
        assert_eq!(read_frame(child.as_mut(), 5_000).await, Some(line(b"r")));
        let pid = child.pid();
        assert!(pid_is_live(pid), "child is alive before the drop");

        drop(child);
        drop(sup);

        // Poll until the leader is gone from /proc: cleanup is bounded
        // (grace+terminate+cleanup ≈ 7s worst case), so plain absence is
        // the check — no group-label assumptions.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        while pid_is_live(pid) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "child {pid} survived dropping its handle and supervisor"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// AGENTKIT-110 lifecycle hardening: stdout EOF is NOT process exit.
    /// A child that closes stdout and keeps running must still be driven
    /// through the shutdown ladder when its handle is abandoned — the EOF
    /// alone must never disarm drop cleanup.
    #[tokio::test]
    async fn stdout_eof_does_not_disarm_drop_cleanup() {
        let dir = TempDir::new().unwrap();
        // Emit one frame, close stdout, then KEEP RUNNING for 30 s.
        let exe = fixture(
            &dir,
            "eof_then_sleep.sh",
            "printf 'r\\n'\nexec 1>&-\nsleep 30",
        );
        let sup = OwnedProcessSupervisor::new();
        let mut child = spawn_child(&sup, exe, &dir, &tight_limits()).await;
        assert_eq!(read_frame(child.as_mut(), 5_000).await, Some(line(b"r")));
        // EOF: the frame channel closes while the process is still alive.
        assert_eq!(child.next_frame().await, None);
        let pid = child.pid();
        assert!(pid_is_live(pid), "child must still be running at EOF");

        drop(child);

        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        while pid_is_live(pid) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "child {pid} survived EOF + drop: EOF wrongly disarmed cleanup"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// AGENTKIT-110 lifecycle hardening: `terminate_owned` touches exactly
    /// the `(pid, generation)` it is given — a sibling child keeps running,
    /// a stale generation is fenced, and unknown identities report `None`.
    #[tokio::test]
    async fn terminate_owned_is_scoped_to_one_generation() {
        let dir = TempDir::new().unwrap();
        let live_script = "sleep 30 & p=$!\ntrap 'kill $p 2>/dev/null; wait $p 2>/dev/null; exit 0' INT\nprintf 'r\\n'\nwait";
        let sup = OwnedProcessSupervisor::new();
        let a = fixture(&dir, "scoped_a.sh", live_script);
        let b = fixture(&dir, "scoped_b.sh", live_script);

        let mut child_a = spawn_child(&sup, a, &dir, &tight_limits()).await;
        assert_eq!(read_frame(child_a.as_mut(), 5_000).await, Some(line(b"r")));
        let (pid_a, gen_a) = (child_a.pid(), child_a.generation());
        let mut child_b = spawn_child(&sup, b, &dir, &tight_limits()).await;
        assert_eq!(read_frame(child_b.as_mut(), 5_000).await, Some(line(b"r")));
        let (pid_b, gen_b) = (child_b.pid(), child_b.generation());

        // Foreign identity and stale generation are refused, nothing dies.
        assert!(
            sup.terminate_owned(pid_a + 1000, gen_a, InterruptPolicy::Quarantine)
                .await
                .is_none()
        );
        assert!(
            sup.terminate_owned(pid_b, gen_b.saturating_sub(1), InterruptPolicy::Quarantine)
                .await
                .is_none()
        );

        // Only A's exact identity is terminated.
        let receipt = sup
            .terminate_owned(pid_a, gen_a, InterruptPolicy::Quarantine)
            .await
            .expect("A is live under its exact identity");
        assert!(receipt.terminated, "{receipt:?}");
        let exit_a = child_a.wait().await;
        assert!(exit_a.after_interrupt, "A was interrupt-terminated");

        // B survived: the whole-runtime drain finds exactly B still live.
        let receipts = sup.drain_owned(InterruptPolicy::HostShutdown).await;
        assert_eq!(receipts.len(), 1, "only B remained owned and live");
        assert!(receipts[0].terminated, "{:?}", receipts[0]);
        let exit_b = child_b.wait().await;
        assert!(exit_b.after_interrupt, "B was terminated only by the drain");

        // Deregistered identities now report None (no re-kill path).
        assert!(
            sup.terminate_owned(pid_a, gen_a, InterruptPolicy::Quarantine)
                .await
                .is_none()
        );
        assert!(
            sup.terminate_owned(pid_b, gen_b, InterruptPolicy::Quarantine)
                .await
                .is_none()
        );
    }

    /// Review round-3 F1: the supervisor Drop is the whole-runtime
    /// shutdown owner. A vendor child whose handle was LEAKED (no OwnedChild
    /// Drop ladder ever runs) must not outlive the retired supervisor.
    #[tokio::test]
    async fn supervisor_drop_sweeps_orphaned_children() {
        let dir = TempDir::new().unwrap();
        let live_script = "sleep 30 & p=$!\ntrap 'kill $p 2>/dev/null; wait $p 2>/dev/null; exit 0' INT\nprintf 'r\\n'\nwait";
        let sup = OwnedProcessSupervisor::new();
        let a = fixture(&dir, "orphan_a.sh", live_script);
        let mut child_a = spawn_child(&sup, a, &dir, &tight_limits()).await;
        assert_eq!(read_frame(child_a.as_mut(), 5_000).await, Some(line(b"r")));
        let pid = child_a.pid();
        assert!(
            pid_is_live(pid),
            "the fixture child is alive before shutdown"
        );

        // Leak the handle: no per-child Drop ladder runs; the supervisor
        // destructor is the only cleanup left standing.
        std::mem::forget(child_a);
        drop(sup);

        // The group SIGKILL is issued synchronously in Drop; poll honestly
        // (reaping is delegated to the still-running reaper task).
        for _ in 0..500 {
            if !pid_is_live(pid) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("orphaned vendor child {pid} outlived the supervisor shutdown");
    }

    /// Whether `pid` is a live (non-zombie) process. A zombie is terminated:
    /// the signal landed and only reaping is pending (the reaper may be a
    /// runtime task that outlives the assertion window).
    fn pid_is_live(pid: u32) -> bool {
        process_state(pid).is_some_and(|state| !state.starts_with('Z'))
    }

    #[cfg(target_os = "linux")]
    fn process_state(pid: u32) -> Option<String> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        stat[stat.rfind(')')? + 1..]
            .split_whitespace()
            .next()
            .map(str::to_owned)
    }

    #[cfg(not(target_os = "linux"))]
    fn process_state(pid: u32) -> Option<String> {
        // SAFETY: signal zero only probes existence of a test-owned process.
        (unsafe { libc::kill(pid as i32, 0) } == 0).then(|| "S".to_owned())
    }

    /// Review round-4 F5 (real OS reproduction): the NORMAL completion path
    /// must not leave descendants running. The leader starts a same-process-
    /// group descendant that keeps appending to a heartbeat file, emits a
    /// valid successful root result and exits 0. `wait()` must reconcile the
    /// owned group (SIGTERM → grace → SIGKILL → verify empty) before it
    /// deregisters, so after the handle and the supervisor are dropped the
    /// descendant is gone and the heartbeat provably stopped growing.
    #[tokio::test]
    async fn normal_completion_reaps_same_group_descendants() {
        let dir = TempDir::new().unwrap();
        let heartbeat = dir.path().join("heartbeat");
        let pidfile = dir.path().join("descendant.pid");
        let result_frame = r#"{"type":"result","subtype":"success","is_error":false,"result":"ok","session_id":"sess-1"}"#;
        // Background job = same process group as the leader. The leader
        // waits for the descendant's first heartbeat byte so the reconcile
        // can never race the descendant's start.
        let script = format!(
            "hb={heartbeat:?}\npidf={pidfile:?}\n( while :; do printf 'x' >> \"$hb\"; sleep 0.1; done ) &\np=$!\nprintf '%s' \"$p\" > \"$pidf\"\nwhile [ ! -s \"$hb\" ]; do :; done\nprintf '%s\\n' '{result_frame}'\nexit 0\n",
        );
        let exe = fixture(&dir, "leader_with_descendant.sh", &script);
        let sup = OwnedProcessSupervisor::new();
        let mut child = spawn_child(&sup, exe, &dir, &tight_limits()).await;

        // The leader's successful root result is delivered intact: the F5
        // reconcile never touches the leader's exit facts.
        match read_frame(child.as_mut(), 5_000).await {
            Some(FrameLine::Line(bytes)) => {
                assert_eq!(String::from_utf8_lossy(&bytes), result_frame);
            }
            other => panic!("expected the successful root result frame, got {other:?}"),
        }
        let pid = child.pid();
        let descendant: u32 = std::fs::read_to_string(&pidfile)
            .expect("descendant recorded its pid")
            .trim()
            .parse()
            .expect("descendant pid is numeric");
        assert!(
            pid_is_live(descendant),
            "the same-group descendant must be running before wait()"
        );

        let exit = child.wait().await;
        assert_eq!(exit.exit_code, Some(0), "leader exit code preserved");
        assert_eq!(exit.signal, None);
        assert!(!exit.after_interrupt, "this is a normal completion");
        assert_eq!(exit.pid, pid);

        // The honest F5 surface: normal completion records what it could
        // prove about the group — through the portable port method and the
        // concrete accessor alike.
        let receipt = sup
            .descendant_cleanup(pid, exit.generation)
            .expect("normal completion records its descendant outcome");
        assert_eq!(
            Some(receipt.clone()),
            sup.descendant_cleanup_report(pid, exit.generation),
            "the trait read-back and the concrete accessor agree"
        );
        assert!(
            sup.descendant_cleanup(pid, exit.generation + 1).is_none(),
            "a stale generation has no report, which is not a claim of clean"
        );
        assert!(receipt.terminated, "{receipt:?}");
        assert_eq!(
            receipt.cleanup_uncertain,
            !kernel_group_absent(pid),
            "{receipt:?}"
        );

        // Exactly the reproduction: drop the child handle and the
        // supervisor, then prove the descendant is dead rather than alive
        // and still writing.
        drop(child);
        drop(sup);

        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while pid_is_live(descendant) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "same-group descendant {descendant} survived normal completion (F5)"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // Two samples across more than one write interval: equal sizes prove
        // the heartbeat stopped, i.e. the writer is really dead.
        let size = |path: &std::path::Path| std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        let before = size(&heartbeat);
        assert!(before > 0, "the descendant wrote its heartbeat first");
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            before,
            size(&heartbeat),
            "descendant kept writing after the group reconcile"
        );
    }
}
