//! One running helper process and everything the launcher holds of it.
//!
//! - The launcher keeps the write end of the helper's stdin; dropping it
//!   (EOF) asks the helper to stop its VM and exit.
//! - A waiter task owns the `Child` and is the only place it is reaped. A
//!   stop that outlasts its grace asks that task to SIGKILL the child; a
//!   pid is never signalled from outside it, so a reaped (and possibly
//!   reused) pid is never hit.
//! - The waiter takes the exit with `wait()` and, should the child signal
//!   be missed, with a `try_wait()` every [`REAP_TICK`]. It keeps the exit
//!   status and logs a non-zero one: a helper exits 0 only when its VM
//!   reached the stopped state, so any other status says the VM's end was
//!   not observed.
//! - A reader task drains stdout continuously and publishes the last
//!   event; stderr goes to the machine's `helper.log`.
//! - A reaped helper is not yet a stopped machine: the VM lives in Apple's
//!   VM service process, so the service also waits for the disks to be
//!   released ([`crate::disks::Disks::wait_released`]).

use std::fmt;
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::sync::{Mutex as StdMutex, PoisonError};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, watch};

use crate::error::bounded;
use crate::helper::{EVENT_LINE_MAX_BYTES, HelperEvent};
use agentkit_sys::{ProcessLookup, lookup_process};

/// How long a SIGKILLed helper gets to be reaped.
const KILL_WAIT: Duration = Duration::from_secs(5);
/// How long writing the configuration line may take.
const CONFIG_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// How often the waiter polls for an exit besides waiting for it.
const REAP_TICK: Duration = Duration::from_millis(250);

/// How a reaped helper ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Exited {
    /// `wait` answered this status.
    Status(ExitStatus),
    /// Another reaper collected it first; its status is unknown.
    Unknown,
}

impl fmt::Display for Exited {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Exited::Status(status) => write!(f, "{status}"),
            Exited::Unknown => f.write_str("status unknown"),
        }
    }
}

/// What the helper reported last on stdout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Phase {
    Starting,
    Started,
    Connected,
    /// `stopped`, a broken line, or stdout closed: the reason.
    Ended(String),
}

/// A running (or exited, not yet stopped) helper.
pub(crate) struct Machine {
    pub(crate) generation: u64,
    pub(crate) pid: u32,
    pub(crate) started: u64,
    /// Never printed: no `Debug` on this type.
    pub(crate) token: String,
    stdin: StdMutex<Option<ChildStdin>>,
    exited: watch::Receiver<Option<Exited>>,
    phase: watch::Receiver<Phase>,
    kill: mpsc::Sender<()>,
}

impl Machine {
    /// Start `helper` with no arguments and `stderr_log` as its stderr.
    /// The caller writes the configuration with [`Machine::configure`].
    pub(crate) fn spawn(
        helper: &Path,
        stderr_log: std::fs::File,
        generation: u64,
        token: String,
    ) -> std::io::Result<Self> {
        let mut child = helper_command(helper, stderr_log).spawn()?;
        let pid = child.id().unwrap_or(0);
        // Not reaped before the waiter task runs, so the pid is still this
        // child's here.
        let started = match lookup_process(pid) {
            Ok(ProcessLookup::Found { started, .. }) => started,
            other => {
                let _ = child.start_kill();
                return Err(std::io::Error::other(format!(
                    "the helper's start time cannot be read: {other:?}"
                )));
            }
        };
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let (phase_tx, phase) = watch::channel(Phase::Starting);
        if let Some(stdout) = stdout {
            tokio::spawn(read_events(stdout, phase_tx));
        }
        let (exited_tx, exited) = watch::channel(None);
        let (kill, kill_rx) = mpsc::channel(1);
        tokio::spawn(wait_for_exit(child, kill_rx, exited_tx, generation));
        Ok(Self {
            generation,
            pid,
            started,
            token,
            stdin: StdMutex::new(stdin),
            exited,
            phase,
            kill,
        })
    }

    /// Write the configuration line and keep stdin open.
    pub(crate) async fn configure(&self, line: &str) -> Result<(), String> {
        let stdin = self.take_stdin();
        let Some(mut stdin) = stdin else {
            return Err("the helper's stdin is closed".into());
        };
        let mut bytes = line.as_bytes().to_vec();
        bytes.push(b'\n');
        let write = async {
            stdin.write_all(&bytes).await?;
            stdin.flush().await
        };
        match tokio::time::timeout(CONFIG_WRITE_TIMEOUT, write).await {
            Ok(Ok(())) => {
                *self.stdin.lock().unwrap_or_else(PoisonError::into_inner) = Some(stdin);
                Ok(())
            }
            Ok(Err(error)) => Err(format!(
                "writing the helper's configuration failed: {error}"
            )),
            Err(_) => Err("writing the helper's configuration timed out".into()),
        }
    }

    /// Wait until the helper reports `connected`, or fail when it reports
    /// anything final or `timeout` passes.
    pub(crate) async fn wait_connected(&self, timeout: Duration) -> Result<(), String> {
        let mut phase = self.phase.clone();
        let reached = phase.wait_for(|p| matches!(p, Phase::Connected | Phase::Ended(_)));
        match tokio::time::timeout(timeout, reached).await {
            Err(_) => Err(format!(
                "the helper did not connect to its guest within {} s",
                timeout.as_secs()
            )),
            Ok(Err(_)) => Err("the helper's output ended".into()),
            Ok(Ok(phase)) => match &*phase {
                Phase::Connected => Ok(()),
                Phase::Ended(reason) => Err(format!("the helper stopped: {reason}")),
                _ => Err("the helper's output ended".into()),
            },
        }
    }

    /// True while the helper process has not been reaped.
    pub(crate) fn running(&self) -> bool {
        self.exited.borrow().is_none()
    }

    /// Close stdin, give the helper `grace` to exit, then SIGKILL it.
    /// `Ok` with how it ended, once the waiter has reaped it.
    pub(crate) async fn stop(&self, grace: Duration) -> Result<Exited, String> {
        drop(self.take_stdin());
        if let Some(exit) = self.wait_exit(grace).await {
            return Ok(exit);
        }
        let _ = self.kill.try_send(());
        if let Some(exit) = self.wait_exit(KILL_WAIT).await {
            return Ok(exit);
        }
        Err(format!(
            "helper pid {} did not exit after SIGKILL",
            self.pid
        ))
    }

    async fn wait_exit(&self, within: Duration) -> Option<Exited> {
        let mut exited = self.exited.clone();
        let reaped = exited.wait_for(Option::is_some);
        match tokio::time::timeout(within, reaped).await {
            Ok(Ok(exit)) => *exit,
            _ => None,
        }
    }

    fn take_stdin(&self) -> Option<ChildStdin> {
        self.stdin
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }
}

/// `helper` with no arguments, piped stdin and stdout, `stderr_log` as
/// stderr, and an environment of `PATH`, `HOME` and `TMPDIR` only: the
/// app's environment may hold secrets and the helper needs none.
fn helper_command(helper: &Path, stderr_log: std::fs::File) -> Command {
    let mut command = Command::new(helper);
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin");
    for name in ["HOME", "TMPDIR"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(stderr_log));
    command
}

/// Own `child` until it is reaped; SIGKILL it when asked. Publishes how it
/// ended once `wait()` or a `try_wait()` tick collected it.
async fn wait_for_exit(
    mut child: Child,
    mut kill: mpsc::Receiver<()>,
    exited: watch::Sender<Option<Exited>>,
    generation: u64,
) {
    let pid = child.id().unwrap_or(0);
    let mut tick = tokio::time::interval(REAP_TICK);
    let mut kill_open = true;
    let outcome = loop {
        tokio::select! {
            outcome = child.wait() => break outcome,
            _ = tick.tick() => match child.try_wait() {
                Ok(None) => {}
                Ok(Some(status)) => break Ok(status),
                Err(error) => break Err(error),
            },
            request = kill.recv(), if kill_open => match request {
                Some(()) => {
                    let _ = child.start_kill();
                }
                None => kill_open = false,
            },
        }
    };
    let exit = match outcome {
        Ok(status) => {
            if !status.success() {
                crate::service::log(&format!(
                    "helper pid {pid} of generation {generation} ended: {status}"
                ));
            }
            Exited::Status(status)
        }
        // ECHILD: another reaper (a later launcher's recovery in this
        // process) collected it, so it is gone.
        Err(error) => {
            crate::service::log(&format!("waiting for helper pid {pid} failed: {error}"));
            Exited::Unknown
        }
    };
    let _ = exited.send(Some(exit));
}

/// Publish each event line of `stdout`; a line over the bound or not an
/// event ends the protocol, and the rest is drained unread.
async fn read_events(stdout: impl AsyncRead + Unpin, phase: watch::Sender<Phase>) {
    let mut reader = BufReader::new(stdout);
    let mut line = Vec::new();
    let ended = loop {
        line.clear();
        let limit = (EVENT_LINE_MAX_BYTES + 1) as u64;
        match (&mut reader).take(limit).read_until(b'\n', &mut line).await {
            Ok(0) => break "the helper's output closed".to_owned(),
            Ok(_) if line.last() != Some(&b'\n') && line.len() > EVENT_LINE_MAX_BYTES => {
                break "the helper wrote an overlong line".to_owned();
            }
            Ok(_) => match serde_json::from_slice::<HelperEvent>(&line) {
                Ok(HelperEvent::Started) => {
                    phase.send_replace(Phase::Started);
                }
                Ok(HelperEvent::Connected) => {
                    phase.send_replace(Phase::Connected);
                }
                Ok(HelperEvent::Stopped { reason }) => break bounded(&reason),
                Err(_) => break "the helper wrote a line that is not an event".to_owned(),
            },
            Err(error) => break format!("reading the helper's output failed: {error}"),
        };
    };
    phase.send_replace(Phase::Ended(ended));
    let _ = tokio::io::copy(&mut reader, &mut tokio::io::sink()).await;
}
