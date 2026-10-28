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

use std::collections::HashMap;
use std::process::Stdio;
use std::process::id as process_id;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{ChildStderr, ChildStdout};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::limits::CodingLimits;
use crate::process_tree::{
    EXEC_ID_ENV, TreeTracker, crate_platform, find_marked_processes, scan_marked,
};
use crate::truncate::{TruncationLimits, format_size, truncate_tail};

/// Kill `pid` and every process in its group.
pub fn kill_process_tree(pid: i32) {
    // group may be gone, or never was one
    unsafe { libc::kill(-pid, libc::SIGSTOP) };
    if unsafe { libc::kill(-pid, libc::SIGKILL) } != 0 {
        // already dead
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
}

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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Teardown {
    /// True when SIGTERM was not enough and SIGKILL was sent.
    pub escalated: bool,
    /// Marked pids still alive after SIGKILL.
    pub survivors: Vec<i32>,
    /// `Unsupported`: descendants outside the group could not be looked for.
    pub scan: TeardownScan,
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

fn signal_pid(pid: i32, sig: i32) {
    // already gone
    unsafe { libc::kill(pid, sig) };
}

fn pid_alive(pid: i32) -> bool {
    let rc = unsafe { libc::kill(pid, 0) };
    if rc == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Owner liveness for markers whose `owner_pid` may exceed a real `pid_t`
/// (TS accepts any positive safe integer; `process.kill(pid, 0)` answers
/// ESRCH/EINVAL for absurd values, i.e. "not alive").
fn owner_alive(pid: i64) -> bool {
    pid <= i32::MAX as i64 && pid_alive(pid as i32)
}

async fn sleep(ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

type FindFn = Arc<dyn Fn() -> BoxFuture<'static, Option<Vec<i32>>> + Send + Sync>;

fn absorb(known: &mut Vec<i32>, scan: &mut TeardownScan, pids: Option<Vec<i32>>) {
    match pids {
        None => *scan = TeardownScan::Unsupported,
        Some(pids) => {
            for pid in pids {
                if !known.contains(&pid) {
                    known.push(pid);
                }
            }
        }
    }
}

/// SIGTERM the group and every marked pid, wait `grace_ms`, SIGKILL what is
/// left, then report what still lives. `seen` counts distinct marked pids.
async fn terminate(pgid: Option<i32>, find: FindFn, grace_ms: u64) -> (Teardown, usize) {
    let mut known: Vec<i32> = Vec::new();
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
    absorb(&mut known, &mut scan, find().await);
    if known.is_empty() && !remaining(&known) {
        return (
            Teardown {
                escalated: false,
                survivors: vec![],
                scan,
            },
            0,
        );
    }
    // Freeze first so nothing can fork between discovery and the signal; rescan
    // until the set stops growing. A stopped process only sees SIGTERM once
    // continued, hence the SIGCONT.
    for _ in 0..3 {
        signal_all(libc::SIGSTOP, &known);
        let before = known.len();
        absorb(&mut known, &mut scan, find().await);
        if known.len() == before {
            break;
        }
    }
    signal_all(libc::SIGTERM, &known);
    signal_all(libc::SIGCONT, &known);
    let deadline = Instant::now() + Duration::from_millis(grace_ms);
    while remaining(&known) && Instant::now() < deadline {
        sleep(POLL_MS).await;
    }
    let escalated = remaining(&known);
    if escalated {
        absorb(&mut known, &mut scan, find().await);
        signal_all(libc::SIGKILL, &known);
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
        if let Some(live) = find().await {
            survivors.retain(|pid| live.contains(pid));
        }
    }
    let seen = known.len();
    (
        Teardown {
            escalated,
            survivors,
            scan,
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
            scan_marked(crate_platform()).await.map(|all| {
                all.into_iter()
                    .filter(|m| !owner_alive(m.owner_pid))
                    .map(|m| m.pid)
                    .collect()
            })
        })
    });
    let grace = grace_ms.unwrap_or(2000).min(KILL_GRACE_CAP_MS);
    match tokio::spawn(terminate(None, find, grace)).await {
        Ok((_, seen)) => seen,
        Err(_) => 0,
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
        teardown: Teardown {
            escalated: false,
            survivors: vec![],
            scan: TeardownScan::Ok,
        },
    }
}

/// Run one process group to completion, timeout or abort.
pub async fn run_bounded(spec: RunSpec) -> RunResult {
    let tail = Arc::new(Mutex::new(Tail::new(
        spec.limits.clone(),
        spec.scratch.clone(),
        spec.log_name.clone(),
    )));
    // Inherited by every descendant, which is how teardown and the crash sweep
    // find the ones that left the process group.
    let exec_id = format!("{}:{}", process_id(), uuid::Uuid::new_v4());
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
    let pid: Option<i32> = child.id().map(|p| p as i32);
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
    let find: FindFn = {
        let exec_id = exec_id.clone();
        let tracker = tracker.clone();
        Arc::new(move || {
            let exec_id = exec_id.clone();
            let tracker = tracker.clone();
            Box::pin(async move {
                let marked = find_marked_processes(&exec_id).await;
                let tracked = match &tracker {
                    Some(tracker) => tracker.sample().await,
                    None => Some(Vec::new()),
                };
                if marked.is_none() && tracked.is_none() {
                    return None;
                }
                let mut set: Vec<i32> = Vec::new();
                for pid in marked
                    .into_iter()
                    .flatten()
                    .chain(tracked.into_iter().flatten())
                {
                    if !set.contains(&pid) {
                        set.push(pid);
                    }
                }
                Some(set)
            })
        })
    };

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
            Err(_) => Teardown {
                escalated: false,
                survivors: vec![],
                scan: TeardownScan::Unsupported,
            },
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::CodingLimits;

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
        assert_eq!(
            result.teardown,
            Teardown {
                escalated: false,
                survivors: vec![],
                scan: TeardownScan::Ok
            }
        );
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
        assert_eq!(
            result.teardown,
            Teardown {
                escalated: false,
                survivors: vec![],
                scan: TeardownScan::Ok
            }
        );
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
        assert_eq!(
            result.teardown,
            Teardown {
                escalated: true,
                survivors: vec![],
                scan: TeardownScan::Ok
            }
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
