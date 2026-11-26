//! `exec`: run shell text as `<shell> -c <command>` in its own session,
//! with a clean environment, `/dev/null` as stdin and one pipe for stdout
//! and stderr, then supervise it to exactly one receipt.
//!
//! Receipts: exit 0 → `succeeded`; another exit or a signal → `failed`;
//! the deadline → `timed_out`; `cancel` (SIGTERM, then SIGKILL after 2 s)
//! → `cancelled`; output past the limit → `output_limit`. The receipt is
//! recorded only after every process that ends with the operation is gone
//! and the copier has drained the pipe, so the log is complete when the
//! state turns terminal. A command that cannot be started (the log or the
//! working directory refused, the shell missing) is recorded as `failed`
//! with exit [`SETUP_FAILED_EXIT`] and a line in its log saying why. A log
//! directory the workload broke is set aside and made again first (see
//! [`crate::output`]); only when that fails too does the command fail.

use std::fs::File;
use std::io::PipeWriter;
use std::os::fd::OwnedFd;
use std::process::{Command, Stdio};
use std::sync::{Arc, PoisonError};
use std::time::Duration;

use agentkit_sandbox_proto::guest::{ExecRequest, GuestErrorCode, OperationState, Reply};
use agentkit_sandbox_proto::{valid_operation_id, valid_sha256};
use agentkit_sys::ChildExit;
use tokio::net::unix::pipe;
use tokio::task::JoinHandle;

use crate::conn::error_reply;
use crate::optable::{Admission, ExecOp, OpEntry, OpKind, failed};
use crate::output::{LOG_MAX_BYTES, OpLog, copy_output};
use crate::paths::{FsError, components, open_root, walk};
use crate::server::Server;

/// Exit status recorded for a command that could not be started.
pub(crate) const SETUP_FAILED_EXIT: i32 = 126;
/// Longest deadline a command may ask for: one hour.
const TIMEOUT_MAX_MS: u64 = 3_600_000;
const TZ_MAX_BYTES: usize = 256;
/// How long the copier may take to drain the pipe after the sweep.
const DRAIN_LIMIT: Duration = Duration::from_secs(2);

/// Handle one `exec` request, then supervise the command to its receipt.
pub(crate) async fn handle_exec(server: Arc<Server>, id: u64, req: ExecRequest) {
    if let Err(reply) = validate(&req) {
        server.out.reply(id, reply).await;
        return;
    }
    let kind = OpKind::Exec(ExecOp::default());
    let entry = match server.ops.admit(&req.op, &req.args_sha256, kind) {
        Admission::New(entry) => entry,
        Admission::Existing(state) => {
            let reply = Reply::Accepted {
                state,
                existing: true,
            };
            server.out.reply(id, reply).await;
            return;
        }
        Admission::Conflict => {
            server.out.reply(id, Reply::Conflict).await;
            return;
        }
    };
    let deadline = Duration::from_millis(req.timeout_ms);
    let started = {
        let (server, entry) = (server.clone(), entry.clone());
        tokio::task::spawn_blocking(move || start(&server, entry.exec()?, &req))
            .await
            .ok()
            .flatten()
    };
    let Some(started) = started else {
        entry.finish(failed(SETUP_FAILED_EXIT));
        let reply = Reply::Accepted {
            state: OperationState::Accepted,
            existing: false,
        };
        server.out.reply(id, reply).await;
        return;
    };
    entry.mark_running();
    let reply = Reply::Accepted {
        state: OperationState::Running,
        existing: false,
    };
    server.out.reply(id, reply).await;
    supervise(&server, &entry, started, deadline).await;
}

fn validate(req: &ExecRequest) -> Result<(), Reply> {
    let invalid = |message: &str| error_reply(GuestErrorCode::InvalidRequest, message);
    if !valid_operation_id(&req.op) || !valid_sha256(&req.args_sha256) {
        return Err(invalid("operation id or argument hash is malformed"));
    }
    if let Err(error) = components(&req.cwd) {
        return Err(error.reply());
    }
    if req.command.contains('\0') {
        return Err(invalid("command contains a NUL byte"));
    }
    if req.timeout_ms == 0 || req.timeout_ms > TIMEOUT_MAX_MS {
        return Err(invalid("timeout_ms is outside 1..=3600000"));
    }
    if req.output_limit.max_bytes > LOG_MAX_BYTES {
        return Err(invalid("output_limit.max_bytes is over 64 MiB"));
    }
    let bad_tz =
        |tz: &String| tz.is_empty() || tz.len() > TZ_MAX_BYTES || tz.contains(char::is_control);
    if req.tz.as_ref().is_some_and(bad_tz) {
        return Err(invalid("tz is empty, too long or has control characters"));
    }
    Ok(())
}

/// A started command.
struct Started {
    pid: u32,
    pipe: pipe::Receiver,
    log: Arc<OpLog>,
}

/// Create the log, enter the working directory and spawn. Blocking.
fn start(server: &Server, exec: &ExecOp, req: &ExecRequest) -> Option<Started> {
    let limit = req.output_limit;
    let log = {
        let _setup = server
            .log_setup
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        OpLog::create(&server.config.workspace_root, &req.op, limit).ok()?
    };
    let log = Arc::new(log);
    let _ = exec.log.set(log.clone());
    match launch(server, req) {
        Ok((pid, pipe)) => Some(Started { pid, pipe, log }),
        Err(why) => {
            log.append(format!("agentkit-sandboxd: {why}\n").as_bytes());
            log.close();
            None
        }
    }
}

fn launch(server: &Server, req: &ExecRequest) -> Result<(u32, pipe::Receiver), String> {
    let names = components(&req.cwd).map_err(|_| "the working directory is invalid".to_owned())?;
    let cwd = open_root(&server.config.workspace_root)
        .and_then(|root| walk(root, &names, true))
        .map_err(|error| format!("cannot enter the working directory: {}", describe(&error)))?;
    let (reader, writer) =
        std::io::pipe().map_err(|error| format!("cannot create the output pipe: {error}"))?;
    let receiver = pipe::Receiver::from_owned_fd(OwnedFd::from(reader))
        .map_err(|error| format!("cannot watch the output pipe: {error}"))?;
    let mut cmd = command(server, req, writer, cwd)
        .map_err(|error| format!("cannot prepare the shell: {error}"))?;
    let pid = server
        .reaper
        .spawn(&mut cmd)
        .map_err(|error| format!("cannot start the shell: {error}"))?;
    // Dropping the command closes the supervisor's copies of the pipe's
    // write end and of the working directory, so the copier sees the end
    // of the output once the command's processes are gone.
    drop(cmd);
    Ok((pid, receiver))
}

fn command(
    server: &Server,
    req: &ExecRequest,
    out: PipeWriter,
    cwd: File,
) -> std::io::Result<Command> {
    let err = out.try_clone()?;
    let mut cmd = Command::new(&server.config.shell);
    cmd.arg("-c")
        .arg(&req.command)
        .env_clear()
        .envs(server.env.iter().map(|(key, value)| (key, value)))
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err);
    if let Some(tz) = &req.tz {
        cmd.env("TZ", tz);
    }
    agentkit_sys::spawn_in_new_session(&mut cmd, cwd)?;
    Ok(cmd)
}

fn describe(error: &FsError) -> &'static str {
    match error {
        FsError::Link => "a path component is a symbolic link",
        FsError::NotADirectory => "a path component is not a directory",
        FsError::NoSpace => "the volume is full",
        _ => "the directory cannot be opened or created",
    }
}

/// What ended the wait for the command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Decision {
    Exited(ChildExit),
    OutputLimit,
    Cancelled,
    TimedOut,
}

async fn supervise(server: &Server, entry: &OpEntry, started: Started, deadline: Duration) {
    let Started { pid, pipe, log } = started;
    let copier = tokio::spawn(copy_output(pipe, log.clone()));
    let decision = decide(server, entry, &log, pid, deadline).await;
    if decision == Decision::Cancelled {
        server.reaper.terminate(pid).await;
    }
    server.reaper.end_operation(pid).await;
    drain(copier).await;
    log.close();
    entry.finish(receipt(decision, log.over_limit()));
}

async fn decide(
    server: &Server,
    entry: &OpEntry,
    log: &OpLog,
    pid: u32,
    deadline: Duration,
) -> Decision {
    let Some(exec) = entry.exec() else {
        return Decision::Cancelled;
    };
    // An exit already there wins over a cancel or deadline that arrives
    // in the same instant.
    tokio::select! {
        biased;
        exit = server.reaper.wait_exit(pid) => Decision::Exited(exit),
        () = log.limit_hit.notified() => Decision::OutputLimit,
        () = exec.cancel.notified() => Decision::Cancelled,
        () = tokio::time::sleep(deadline) => Decision::TimedOut,
    }
}

/// Wait for the copier to see the end of the pipe; a process that left
/// the operation's group may still hold it open (Embedded mode), so the
/// wait is bounded and the copier stopped after it.
async fn drain(mut copier: JoinHandle<()>) {
    if tokio::time::timeout(DRAIN_LIMIT, &mut copier)
        .await
        .is_err()
    {
        copier.abort();
        let _ = copier.await;
    }
}

/// The receipt. Output past the limit is decided by the log, not by which
/// event woke the supervisor first: the limit may be passed while the
/// command is already exiting.
fn receipt(decision: Decision, over_limit: bool) -> OperationState {
    match decision {
        Decision::Cancelled => OperationState::Cancelled,
        Decision::TimedOut => OperationState::TimedOut,
        _ if over_limit => OperationState::OutputLimit,
        Decision::OutputLimit => OperationState::OutputLimit,
        Decision::Exited(exit) => from_exit(exit),
    }
}

fn from_exit(exit: ChildExit) -> OperationState {
    match exit {
        ChildExit::Exited { code: 0 } => OperationState::Succeeded { exit: 0 },
        ChildExit::Exited { code } => failed(code),
        ChildExit::Killed { signal } | ChildExit::Dumped { signal } => OperationState::Failed {
            exit: None,
            signal: Some(signal),
        },
        ChildExit::Running | ChildExit::NoSuchChild => OperationState::Failed {
            exit: None,
            signal: None,
        },
    }
}
