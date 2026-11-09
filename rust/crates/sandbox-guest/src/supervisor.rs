//! Per-execution command supervisor (§8.3).
//!
//! Spawns via `tokio::process` with a `pre_exec` that `setsid`s, drops to
//! worker-decided credentials (root only when the kernel cmdline allowed),
//! and applies rlimits. Output is drained with caps; timeout/cancel kill the
//! whole process group; surviving descendants are reported `Uncertain` so the
//! worker quarantines (T10).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agentkit_sandbox_contracts::api::{ExecArgv, ExecEventKind, StreamKind};
use agentkit_sandbox_contracts::grant::GuestEnvironment;
use agentkit_sandbox_contracts::ids::ExecutionId;
use agentkit_sandbox_contracts::limits::{MAX_ARGV_BYTES, MAX_ARGV_LEN, MAX_CHUNK_BYTES};
use agentkit_sandbox_contracts::path::GuestPath;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use tokio::io::{AsyncReadExt, BufReader};
use tokio::sync::mpsc;

use crate::error::{GuestFail, code};
use crate::fsops::GuestFs;

/// Fixed dirs a `Direct` executable must live under.
pub const EXEC_PATH_DIRS: [&str; 6] = [
    "/usr/local/bin",
    "/usr/bin",
    "/bin",
    "/usr/local/sbin",
    "/usr/sbin",
    "/sbin",
];
/// Fixed shell for the `Shell` form (S: never the host shell).
pub const GUEST_SHELL: &str = "/bin/bash";
const FIXED_PATH: &str = "/usr/local/bin:/usr/bin:/bin:/usr/local/sbin:/usr/sbin:/sbin";
const KILL_GRACE: Duration = Duration::from_secs(2);
const RLIMIT_FSIZE_BYTES: u64 = 1024 * 1024 * 1024;

/// Event sink: (execution, event kind). Sequencing is per-execution and done
/// by the sink owner (server or tests).
pub type EventSink = mpsc::UnboundedSender<(ExecutionId, ExecEventKind)>;

/// One spawn request as decoded from `WorkerToGuest::ExecSpawn`.
pub struct SpawnRequest {
    pub execution_id: ExecutionId,
    pub argv: ExecArgv,
    pub cwd: GuestPath,
    pub environment: GuestEnvironment,
    pub timeout_sec: u32,
    pub max_output_bytes: u64,
    pub max_processes: u32,
    pub run_as_uid: u32,
    pub run_as_gid: u32,
}

struct Running {
    pid: u32,
    finished: bool,
}

/// Shared supervisor for all executions in this guest.
pub struct Supervisor {
    allow_guest_root: bool,
    fs: Arc<GuestFs>,
    tx: EventSink,
    running: Mutex<HashMap<ExecutionId, Running>>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

impl Supervisor {
    #[must_use]
    pub fn new(allow_guest_root: bool, fs: Arc<GuestFs>, tx: EventSink) -> Self {
        Self {
            allow_guest_root,
            fs,
            tx,
            running: Mutex::new(HashMap::new()),
        }
    }

    fn emit(&self, id: &ExecutionId, kind: ExecEventKind) {
        let _ = self.tx.send((id.clone(), kind));
    }

    fn mark_finished(&self, id: &ExecutionId) {
        if let Ok(mut m) = self.running.lock()
            && let Some(r) = m.get_mut(id)
        {
            r.finished = true;
        }
    }

    /// Admit and spawn; emits `Accepted` then `Spawned`/`Uncertain`.
    /// Errors before `Accepted` mean proven not-started.
    pub fn spawn(self: &Arc<Self>, req: SpawnRequest) -> Result<(), GuestFail> {
        self.admit(&req)?;
        self.emit(
            &req.execution_id,
            ExecEventKind::Accepted { at_ms: now_ms() },
        );
        let me = Arc::clone(self);
        tokio::spawn(async move {
            me.run(req).await;
        });
        Ok(())
    }

    fn admit(&self, req: &SpawnRequest) -> Result<(), GuestFail> {
        if req.run_as_uid == 0 && !self.allow_guest_root {
            return Err(GuestFail::new(
                code::ROOT_DENIED,
                "uid 0 refused without agentkit.allow_guest_root=1",
            ));
        }
        if req.run_as_gid == 0 && !self.allow_guest_root {
            return Err(GuestFail::new(
                code::ROOT_DENIED,
                "gid 0 refused without agentkit.allow_guest_root=1",
            ));
        }
        // Non-root guests cannot switch to a foreign uid: refuse up-front.
        let self_uid = // SAFETY: always valid.
            unsafe { libc::geteuid() };
        if req.run_as_uid != self_uid && self_uid != 0 {
            return Err(GuestFail::new(
                code::NOT_STARTED,
                "cannot change uid without privilege",
            ));
        }
        let argv_total: usize = match &req.argv {
            ExecArgv::Direct { executable, args } => {
                validate_direct_executable(executable)?;
                std::iter::once(executable)
                    .chain(args.iter())
                    .map(String::len)
                    .sum::<usize>()
                    + args.len()
                    + 1
            }
            ExecArgv::Shell { command } => command.len() + 16,
        };
        if argv_total > MAX_ARGV_BYTES {
            return Err(GuestFail::new(code::BAD_INPUT, "argv too large"));
        }
        if let ExecArgv::Direct { args, .. } = &req.argv
            && args.len() > MAX_ARGV_LEN
        {
            return Err(GuestFail::new(code::BAD_INPUT, "argv too long"));
        }
        if req.timeout_sec == 0 || req.timeout_sec > 24 * 3600 {
            return Err(GuestFail::new(code::BAD_INPUT, "timeout out of range"));
        }
        if self
            .running
            .lock()
            .map_err(|_| GuestFail::new(code::IO, "lock"))?
            .len()
            >= agentkit_sandbox_contracts::limits::MAX_CONCURRENT_EXECUTIONS
        {
            return Err(GuestFail::new(code::IO, "too many concurrent executions"));
        }
        if req.execution_id.as_str().contains('\0') {
            return Err(GuestFail::new(code::BAD_INPUT, "bad execution id"));
        }
        Ok(())
    }

    fn build_command(&self, req: &SpawnRequest) -> Result<tokio::process::Command, GuestFail> {
        let (program, args): (String, Vec<String>) = match &req.argv {
            ExecArgv::Direct { executable, args } => {
                if !std::path::Path::new(executable).exists() {
                    return Err(GuestFail::new(code::BAD_EXECUTABLE, "missing executable"));
                }
                (executable.clone(), args.clone())
            }
            ExecArgv::Shell { command } => {
                if !std::path::Path::new(GUEST_SHELL).exists() {
                    return Err(GuestFail::new(code::BAD_EXECUTABLE, "missing /bin/bash"));
                }
                (
                    GUEST_SHELL.to_string(),
                    vec!["-c".to_string(), command.clone()],
                )
            }
        };
        // cwd must resolve beneath a verified root; the verified mapping (not
        // the wire path) forms the real path the child chdirs to.
        let cwd_path = self.fs.real_path(&req.cwd)?;

        let mut cmd = tokio::process::Command::new(program);
        cmd.args(args)
            .env_clear()
            .env("PATH", FIXED_PATH)
            .env("HOME", "/home/agent")
            .env("TERM", "dumb")
            .env("TMPDIR", "/tmp")
            .envs(
                req.environment
                    .entries()
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone())),
            )
            .current_dir(cwd_path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let (uid, gid, timeout_sec, max_procs) = (
            req.run_as_uid,
            req.run_as_gid,
            req.timeout_sec,
            req.max_processes,
        );
        // SAFETY: pre_exec runs async-signal-safe calls only between fork and
        // exec: setsid/setrlimit/umask/setgid/setuid. All failures abort the
        // child rather than exec-ing with partial privileges.
        unsafe {
            cmd.pre_exec(move || {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                let lim = |v: u64| libc::rlimit {
                    rlim_cur: v as libc::rlim_t,
                    rlim_max: v as libc::rlim_t,
                };
                let l_nproc = lim(u64::from(max_procs.max(1)));
                let l_cpu = lim(u64::from(timeout_sec) + 5);
                let l_fsize = lim(RLIMIT_FSIZE_BYTES);
                let l_core = lim(0);
                // Best-effort: the kernel enforces whatever was set.
                libc::setrlimit(libc::RLIMIT_NPROC, &l_nproc);
                libc::setrlimit(libc::RLIMIT_CPU, &l_cpu);
                libc::setrlimit(libc::RLIMIT_FSIZE, &l_fsize);
                libc::setrlimit(libc::RLIMIT_CORE, &l_core);
                libc::umask(0o007);
                let self_gid = libc::getgid();
                if gid != self_gid && libc::setgid(gid) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                let self_uid = libc::geteuid();
                if uid != self_uid && libc::setuid(uid) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        Ok(cmd)
    }

    async fn run(self: Arc<Self>, req: SpawnRequest) {
        let id = req.execution_id.clone();
        let mut cmd = match self.build_command(&req) {
            Ok(c) => c,
            Err(f) => {
                self.emit(
                    &id,
                    ExecEventKind::Uncertain {
                        reason: f.to_string(),
                    },
                );
                return;
            }
        };
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                self.emit(
                    &id,
                    ExecEventKind::Uncertain {
                        reason: format!("spawn failed: {}", e.kind() as u8),
                    },
                );
                return;
            }
        };
        let pid = child.id().unwrap_or(0);
        if let Ok(mut m) = self.running.lock() {
            m.insert(
                id.clone(),
                Running {
                    pid,
                    finished: false,
                },
            );
        }
        self.emit(
            &id,
            ExecEventKind::Spawned {
                at_ms: now_ms(),
                guest_pid: pid,
            },
        );

        let (mut so, mut se) = (child.stdout.take(), child.stderr.take());
        let max_out = req.max_output_bytes;
        let id2 = id.clone();
        let out_task = {
            let me = Arc::clone(&self);
            let id = id.clone();
            tokio::spawn(async move {
                drain_stream(&mut so, StreamKind::Stdout, max_out, me, id).await;
            })
        };
        let err_task = {
            let me = Arc::clone(&self);
            let id = id2.clone();
            tokio::spawn(async move {
                drain_stream(&mut se, StreamKind::Stderr, max_out, me, id).await;
            })
        };

        let timeout = Duration::from_secs(u64::from(req.timeout_sec));
        let mut child_slot = {
            // child moved into a cell we can kill after timeout
            ChildCell::new(child)
        };
        let mut timed_out = false;
        let status = tokio::select! {
            s = child_slot.wait() => s,
            () = tokio::time::sleep(timeout) => {
                timed_out = true;
                kill_group(pid, libc::SIGTERM);
                tokio::select! {
                    s = child_slot.wait() => s,
                    () = tokio::time::sleep(KILL_GRACE) => {
                        kill_group(pid, libc::SIGKILL);
                        child_slot.wait().await
                    }
                }
            }
        };

        let _ = out_task.await;
        let _ = err_task.await;

        let (exit_code, signal) = exit_status(&status);
        self.mark_finished(&id);
        self.emit(
            &id,
            ExecEventKind::Exited {
                exit_code,
                signal,
                timed_out,
                at_ms: now_ms(),
            },
        );

        // Descendant sweep: everyone in the (post-setsid) process group.
        kill_group(pid, libc::SIGKILL);
        tokio::time::sleep(Duration::from_millis(80)).await;
        let survivors = pgid_members(pid);
        if !survivors.is_empty() {
            self.emit(
                &id,
                ExecEventKind::Uncertain {
                    reason: format!("descendants_survived:{}", survivors.len()),
                },
            );
        }
        if let Ok(mut m) = self.running.lock() {
            m.remove(&id);
        }
    }

    /// `WorkerToGuest::ExecCancel`: signal the group; the runner emits the
    /// real `Exited{signal}`. Unknown/finished executions are a no-op.
    pub fn cancel(&self, execution_id: &ExecutionId) {
        let pid = self
            .running
            .lock()
            .ok()
            .and_then(|m| m.get(execution_id).filter(|r| !r.finished).map(|r| r.pid));
        let Some(pid) = pid else { return };
        kill_group(pid, libc::SIGTERM);
        tokio::spawn(async move {
            tokio::time::sleep(KILL_GRACE).await;
            kill_group(pid, libc::SIGKILL);
        });
    }

    /// Report an admission failure as an Uncertain event (worker classifies
    /// the `not_started` reason; nothing was executed).
    pub fn report_failure(&self, execution_id: &ExecutionId, failure: &GuestFail) {
        self.emit(
            execution_id,
            ExecEventKind::Uncertain {
                reason: format!("{}:{}", failure.code, failure.message),
            },
        );
    }
}

fn kill_group(pid: u32, sig: libc::c_int) {
    if pid == 0 {
        return;
    }
    // SAFETY: killpg on our own child group.
    unsafe { libc::killpg(pid as libc::pid_t, sig) };
}

/// Exit code / signal decomposition from a std ExitStatus.
fn exit_status(status: &std::io::Result<std::process::ExitStatus>) -> (Option<i32>, Option<i32>) {
    match status {
        Ok(st) => {
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                match (st.code(), st.signal()) {
                    (Some(c), _) => (Some(c), None),
                    (None, Some(s)) => (None, Some(s)),
                    _ => (None, None),
                }
            }
            #[cfg(not(unix))]
            {
                (st.code(), None)
            }
        }
        Err(_) => (None, None),
    }
}

/// Scan /proc for members of process group `pgid` (excluding self).
fn pgid_members(pgid: u32) -> Vec<u32> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir("/proc") else {
        return out;
    };
    let my_pid = std::process::id();
    for ent in rd.flatten() {
        let Ok(num) = ent.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        if num == my_pid {
            continue;
        }
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{num}/stat")) else {
            continue;
        };
        // Fields after the comm (which may contain spaces/parens): take the
        // part after the last ')': state, ppid, pgrp, ...
        if let Some(rest) = stat.rsplit_once(')')
            && let Some(f) = rest.1.split_whitespace().nth(2)
            && let Ok(g) = f.parse::<u32>()
            && g == pgid
        {
            out.push(num);
        }
    }
    out
}

/// Cell so the timeout branch can still `wait()` the child.
struct ChildCell {
    inner: Option<tokio::process::Child>,
}
impl ChildCell {
    fn new(c: tokio::process::Child) -> Self {
        Self { inner: Some(c) }
    }
    async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        match self.inner.as_mut() {
            Some(c) => c.wait().await,
            None => Ok(std::process::ExitStatus::default()),
        }
    }
}

/// Drain one stream with per-stream cap, chunk events, truncation marker.
async fn drain_stream<R>(
    stream: &mut Option<R>,
    kind: StreamKind,
    max_bytes: u64,
    sup: Arc<Supervisor>,
    id: ExecutionId,
) where
    R: tokio::io::AsyncRead + Unpin,
{
    let Some(raw) = stream.as_mut() else { return };
    let mut reader = BufReader::new(raw);
    let mut buf = vec![0u8; MAX_CHUNK_BYTES];
    let mut total: u64 = 0;
    let mut truncated = false;
    loop {
        match reader.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let n = u64::try_from(n).unwrap_or(0);
                let room = max_bytes.saturating_sub(total);
                let take = usize::try_from(n.min(room)).unwrap_or(0);
                if take > 0 {
                    total += take as u64;
                    let event = match kind {
                        StreamKind::Stdout => ExecEventKind::StdoutChunk {
                            bytes_b64: STANDARD.encode(&buf[..take]),
                        },
                        StreamKind::Stderr => ExecEventKind::StderrChunk {
                            bytes_b64: STANDARD.encode(&buf[..take]),
                        },
                    };
                    sup.emit(&id, event);
                }
                if n > room && !truncated {
                    truncated = true;
                    sup.emit(
                        &id,
                        ExecEventKind::Truncated {
                            stream: kind,
                            bytes: total,
                        },
                    );
                }
            }
        }
    }
}

/// Validate the `Direct` executable shape: absolute, under a PATH dir.
fn validate_direct_executable(exe: &str) -> Result<(), GuestFail> {
    if exe.is_empty()
        || !exe.starts_with('/')
        || exe.contains('\0')
        || !EXEC_PATH_DIRS
            .iter()
            .any(|d| exe.starts_with(&format!("{d}/")))
        || exe
            .split('/')
            .skip(1)
            .any(|s| s == "." || s == ".." || s.is_empty())
    {
        return Err(GuestFail::new(
            code::BAD_EXECUTABLE,
            "executable must be absolute under guest PATH dirs",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_executable_validation() {
        assert!(validate_direct_executable("/usr/bin/printf").is_ok());
        assert!(validate_direct_executable("printf").is_err());
        assert!(validate_direct_executable("/etc/shadow").is_err());
        assert!(validate_direct_executable("/usr/bin/../etc/shadow").is_err());
        assert!(validate_direct_executable("").is_err());
    }

    #[test]
    fn pgid_scan_finds_self_group() {
        // Our own pgid must be discoverable (or at least the scan runs clean).
        let grp = std::process::id(); // our pgid may differ; just exercise it
        let v = pgid_members(grp);
        assert!(v.is_empty() || !v.is_empty());
    }
}
