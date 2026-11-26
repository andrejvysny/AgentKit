//! The supervisor's children: spawning, collecting exits, and ending
//! processes when an operation ends, as the [`ProcessMode`] allows.
//!
//! Exits are collected with `waitpid`, never through `std::process::Child`
//! or `tokio::process`: as PID 1 (and as a subreaper) every orphan of the
//! container becomes the supervisor's child and must be reaped, so one
//! reaper owns all of them and hands each operation its leader's status.
//! A pid is registered under the same lock that reaping takes, before any
//! reap can collect it.
//!
//! What ends with an operation:
//! - [`ProcessMode::Init`]: `kill(-1, SIGKILL)` and reap, repeated until
//!   no child is left, so a process forking during the sweep cannot
//!   escape it. Every process but the supervisor ends, including those of
//!   another operation the host should not have run beside it.
//! - [`ProcessMode::Supervisor`]: the operation's process group and
//!   leader, then every descendant from `/proc`, killed and reaped until
//!   none is left (Linux). Orphans are descendants only while the process
//!   is a child subreaper ([`crate::harden_process`]); on a platform
//!   without `/proc` the group kill is all there is, while a `/proc` that
//!   cannot be read fails closed.
//! - [`ProcessMode::Embedded`]: the operation's process group and leader;
//!   the leader stays an unreaped zombie until then, so its pid (and the
//!   group number) cannot be reused by an unrelated process.
//!
//! A sweep that cannot finish fails closed outside Embedded mode: the
//! supervisor exits (as PID 1 that ends the whole container) rather than
//! record a receipt while a process of the operation may still run.

use std::collections::HashMap;
use std::io;
use std::process::{Child, Command};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use agentkit_sys::{ChildExit, ProcessGroup, Reaped, Signal};
use tokio::time::{sleep, timeout};

use crate::config::ProcessMode;

const POLL: Duration = Duration::from_millis(10);
const BACKGROUND_POLL: Duration = Duration::from_millis(50);
const SWEEP_LIMIT: Duration = Duration::from_secs(10);
const SWEEP_POLL: Duration = Duration::from_millis(5);
/// Time a cancelled command has between SIGTERM and SIGKILL.
const TERM_GRACE: Duration = Duration::from_secs(2);
const KILL_WAIT: Duration = Duration::from_secs(1);
const KILL_ATTEMPTS: u32 = 10;
/// Exit status when processes outlive a sweep.
const EXIT_SWEEP_FAILED: i32 = 70;

#[derive(Default)]
struct Children {
    /// Registered leaders and, once collected, how they ended.
    slots: HashMap<u32, Option<ChildExit>>,
    /// Set at shutdown; no child starts after it.
    closed: bool,
}

/// Spawns, reaps and kills the supervisor's children.
pub(crate) struct Reaper {
    mode: ProcessMode,
    children: Mutex<Children>,
}

impl Reaper {
    pub(crate) fn new(mode: ProcessMode) -> Self {
        Self {
            mode,
            children: Mutex::default(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Children> {
        self.children.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn reaps_all(&self) -> bool {
        self.mode != ProcessMode::Embedded
    }

    /// Spawn `cmd` and register its pid. Refused once shutdown began.
    pub(crate) fn spawn(&self, cmd: &mut Command) -> io::Result<u32> {
        let mut children = self.lock();
        if children.closed {
            return Err(io::Error::other("the supervisor is shutting down"));
        }
        let pid = pid_of(cmd.spawn()?);
        children.slots.insert(pid, None);
        Ok(pid)
    }

    /// Reap every ended child (Init and Supervisor modes only), keeping
    /// the status of registered leaders. True while children remain.
    pub(crate) fn reap_all(&self) -> bool {
        if !self.reaps_all() {
            return false;
        }
        let mut children = self.lock();
        loop {
            match agentkit_sys::reap_any() {
                Ok(Reaped::Child { pid, exit }) => {
                    if let Some(slot) = children.slots.get_mut(&pid) {
                        *slot = Some(exit);
                    }
                }
                Ok(Reaped::NoneReady) | Err(_) => return true,
                Ok(Reaped::NoChildren) => return false,
            }
        }
    }

    /// Reap orphans for as long as the supervisor runs (Init and
    /// Supervisor modes).
    pub(crate) async fn reap_in_background(self: Arc<Self>) {
        loop {
            self.reap_all();
            sleep(BACKGROUND_POLL).await;
        }
    }

    fn poll_exit(&self, pid: u32) -> Option<ChildExit> {
        if self.reaps_all() {
            self.reap_all();
            return self.lock().slots.get(&pid).copied().flatten();
        }
        match agentkit_sys::peek_child_exit(pid) {
            Ok(ChildExit::Running) | Err(_) => None,
            Ok(exit) => Some(exit),
        }
    }

    /// How the leader `pid` ended, once it has. In Embedded mode it is
    /// left unreaped until [`Reaper::end_operation`].
    pub(crate) async fn wait_exit(&self, pid: u32) -> ChildExit {
        loop {
            if let Some(exit) = self.poll_exit(pid) {
                return exit;
            }
            sleep(POLL).await;
        }
    }

    fn release(&self, pid: u32) {
        if !self.reaps_all() {
            let _ = agentkit_sys::reap_child(pid);
        }
        self.lock().slots.remove(&pid);
    }

    /// SIGTERM to the operation's group, then up to [`TERM_GRACE`] for its
    /// leader to end.
    pub(crate) async fn terminate(&self, pid: u32) {
        signal_group(pid, Signal::Term);
        let _ = timeout(TERM_GRACE, self.wait_exit(pid)).await;
    }

    /// End the operation led by `pid` and every process the mode ends with
    /// it, wait until the leader is gone, and release it.
    pub(crate) async fn end_operation(&self, pid: u32) {
        for _ in 0..KILL_ATTEMPTS {
            self.sweep(&[pid]).await;
            if timeout(KILL_WAIT, self.wait_exit(pid)).await.is_ok() {
                self.release(pid);
                return;
            }
        }
        self.fail_closed("an operation's leader outlived SIGKILL");
        self.release(pid);
    }

    /// Shutdown: refuse new children, then end every one there is.
    pub(crate) async fn end_all(&self) {
        let pids: Vec<u32> = {
            let mut children = self.lock();
            children.closed = true;
            children.slots.keys().copied().collect()
        };
        self.sweep(&pids).await;
    }

    async fn sweep(&self, leaders: &[u32]) {
        match self.mode {
            ProcessMode::Init => self.kill_everything().await,
            ProcessMode::Supervisor => self.kill_descendants(leaders).await,
            ProcessMode::Embedded => {
                for pid in leaders {
                    kill_leader_and_group(*pid);
                }
            }
        }
    }

    async fn kill_everything(&self) {
        let deadline = Instant::now() + SWEEP_LIMIT;
        loop {
            if let Err(error) = agentkit_sys::kill_all_processes(Signal::Kill) {
                self.fail_closed(&format!("kill(-1) failed: {error}"));
                return;
            }
            if !self.reap_all() {
                return;
            }
            if Instant::now() >= deadline {
                self.fail_closed("processes outlived kill(-1)");
                return;
            }
            sleep(SWEEP_POLL).await;
        }
    }

    /// The leaders' groups first: an orphan is a descendant only while
    /// the process is a child subreaper, and the group kill reaches the
    /// operation's processes either way. Then every descendant `/proc`
    /// shows, until none is left.
    async fn kill_descendants(&self, leaders: &[u32]) {
        self.kill_groups(leaders);
        let me = std::process::id();
        let deadline = Instant::now() + SWEEP_LIMIT;
        loop {
            let found = match agentkit_sys::descendants(me) {
                Ok(found) => found,
                Err(error) if !scan_failure_is_fatal(&error) => return,
                Err(error) => {
                    self.fail_closed(&format!("cannot list descendants: {error}"));
                    return;
                }
            };
            for pid in &found {
                let _ = agentkit_sys::signal_process(*pid, Signal::Kill);
            }
            self.reap_all();
            if found.is_empty() {
                return;
            }
            if Instant::now() >= deadline {
                self.fail_closed("descendants outlived SIGKILL");
                return;
            }
            sleep(SWEEP_POLL).await;
        }
    }

    /// Kill each leader's group, and the leader itself while it is not
    /// reaped. A group number is not reused while the group has members,
    /// so the group kill is safe after the leader was reaped; the lock
    /// keeps the reaper from collecting a leader (and its pid being
    /// reused) between the check and the leader kill.
    fn kill_groups(&self, leaders: &[u32]) {
        let children = self.lock();
        for pid in leaders {
            signal_group(*pid, Signal::Kill);
            if children.slots.get(pid) == Some(&None) {
                let _ = agentkit_sys::signal_process(*pid, Signal::Kill);
            }
        }
    }

    fn fail_closed(&self, why: &str) {
        eprintln!("agentkit-sandboxd: {why}");
        if self.mode != ProcessMode::Embedded {
            eprintln!("agentkit-sandboxd: exiting so no process of the operation outlives it");
            std::process::exit(EXIT_SWEEP_FAILED);
        }
    }
}

/// Whether a failed `/proc` scan fails the sweep closed. Only a platform
/// without `/proc` (`Unsupported`) is spared: there the group kill is all
/// there is. Anywhere else a descendant could be missed, so the sweep
/// cannot claim to have finished.
fn scan_failure_is_fatal(error: &io::Error) -> bool {
    error.kind() != io::ErrorKind::Unsupported
}

/// The pid of a child whose exit the reaper collects with `waitpid`;
/// std's handle is never waited on.
fn pid_of(child: Child) -> u32 {
    child.id()
}

fn signal_group(pid: u32, signal: Signal) {
    if let Ok(group) = ProcessGroup::from_leader_pid(pid) {
        let _ = group.signal(signal);
    }
}

fn kill_leader_and_group(pid: u32) {
    signal_group(pid, Signal::Kill);
    // The leader may have left its group (`setpgid`, `setsid`).
    let _ = agentkit_sys::signal_process(pid, Signal::Kill);
}

#[cfg(test)]
mod tests;
