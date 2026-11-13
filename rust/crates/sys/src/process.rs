//! OS process-group primitives.
//!
//! A [`ProcessGroup`] is a handle to a child process group the caller
//! spawned with `spawn_in_new_group*` (pgid == child pid). Construction
//! refuses privileged and self targets, mirroring the refusal rules the
//! claude transport enforced at every kill since AGENTKIT-110: no group
//! `0`/`1`, no group above `i32::MAX`, never the caller's own pid and
//! never the caller's own process group. A signal to a group that is
//! already gone is a success [`SignalOutcome::AlreadyGone`], not an error.

use std::io;

/// Errors from process-group control. Per ADR 0021 R7 the discriminant is
/// the outcome class; strings are context only.
#[derive(Debug, thiserror::Error)]
pub enum SysError {
    /// The caller asked to signal a group it must not own (privileged,
    /// self pid or self group).
    #[error("refusing to signal process group {pgid}")]
    Refused {
        /// The rejected process-group id.
        pgid: u32,
    },
    /// `kill(2)` failed for a reason other than ESRCH.
    #[error("kill(-{pgid}, {signal}) failed: {source}")]
    Signal {
        /// The signalled process-group id.
        pgid: u32,
        /// The signal number passed to `kill(2)`.
        signal: i32,
        /// The OS error `kill(2)` reported.
        source: io::Error,
    },
    /// The platform does not implement POSIX process groups.
    #[error("process-group control is unsupported on this platform")]
    Unsupported,
}

/// Signals the supervisor layer sends to owned process groups.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Signal {
    /// Interrupt (ladder rung 1).
    Int,
    /// Terminate (ladder rung 2).
    Term,
    /// Kill (ladder rung 3; unrecoverable).
    Kill,
}

#[cfg(unix)]
impl Signal {
    fn number(self) -> i32 {
        match self {
            Signal::Int => libc::SIGINT,
            Signal::Term => libc::SIGTERM,
            Signal::Kill => libc::SIGKILL,
        }
    }
}

/// Outcome of a delivered signal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SignalOutcome {
    /// The kernel accepted the signal for at least one live member.
    Delivered,
    /// The whole group was already gone (ESRCH): nothing to kill, not an
    /// uncertainty.
    AlreadyGone,
}

/// An owned child process group, constructed from the pid of the group
/// leader the caller spawned (pgid == leader pid).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessGroup {
    pgid: u32,
}

impl ProcessGroup {
    /// Adopt the process group led by `pgid`, typically the pid of a child
    /// spawned with [`spawn_in_new_group`] / [`spawn_in_new_group_tokio`].
    ///
    /// Refuses privileged groups (`0`, `1`), ids above `i32::MAX`, the
    /// caller's own pid and the caller's own process group, so a corrupted
    /// pid can never turn a supervisor kill into a self- or init-kill.
    #[cfg(unix)]
    pub fn from_leader_pid(pgid: u32) -> Result<Self, SysError> {
        if pgid <= 1
            || pgid > i32::MAX as u32
            || pgid == std::process::id()
            || pgid == own_process_group()
        {
            return Err(SysError::Refused { pgid });
        }
        Ok(Self { pgid })
    }

    /// Non-POSIX platforms have no process groups: every adoption is
    /// refused explicitly rather than silently ignored.
    #[cfg(not(unix))]
    pub fn from_leader_pid(_pgid: u32) -> Result<Self, SysError> {
        Err(SysError::Unsupported)
    }

    /// Send `sig` to every member of the group.
    #[cfg(unix)]
    pub fn signal(&self, sig: Signal) -> Result<SignalOutcome, SysError> {
        // SAFETY: `self.pgid` was validated by `from_leader_pid` — a
        // positive id owned by us that is neither our pid nor our group —
        // so the negated value addresses an owned process group, and
        // `kill(2)` reads no caller memory.
        let rc = unsafe { libc::kill(-(self.pgid as i32), sig.number()) };
        if rc == 0 {
            return Ok(SignalOutcome::Delivered);
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(SignalOutcome::AlreadyGone);
        }
        Err(SysError::Signal {
            pgid: self.pgid,
            signal: sig.number(),
            source: error,
        })
    }

    /// Non-POSIX platforms cannot deliver group signals.
    #[cfg(not(unix))]
    pub fn signal(&self, _sig: Signal) -> Result<SignalOutcome, SysError> {
        Err(SysError::Unsupported)
    }

    /// Whether the kernel reports the whole group absent (ESRCH from
    /// `kill(-pgid, 0)`). A group that cannot be probed is reported as
    /// NOT gone — uncertainty never reads as quiet.
    #[cfg(unix)]
    #[must_use]
    pub fn is_gone(&self) -> bool {
        // SAFETY: signal zero probes an owned, validated group without
        // delivering a signal; see `signal`.
        let rc = unsafe { libc::kill(-(self.pgid as i32), 0) };
        rc != 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    }

    /// Non-POSIX platforms cannot probe group liveness: report alive.
    #[cfg(not(unix))]
    #[must_use]
    pub fn is_gone(&self) -> bool {
        false
    }
}

/// Run the child in a brand-new process group (pgid == child pid), so a
/// later [`ProcessGroup`] signal reaches the whole tree the child forks.
/// No-op on platforms without POSIX process groups, matching the callers'
/// previous `#[cfg(unix)]` gating.
pub fn spawn_in_new_group(cmd: &mut std::process::Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(not(unix))]
    let _ = cmd;
}

/// [`spawn_in_new_group`] for `tokio::process::Command`.
pub fn spawn_in_new_group_tokio(cmd: &mut tokio::process::Command) {
    #[cfg(unix)]
    cmd.process_group(0);
    #[cfg(not(unix))]
    let _ = cmd;
}

/// The caller's own process group id (`getpgrp(2)`). On platforms without
/// process groups this reports the caller's pid.
#[cfg(unix)]
#[must_use]
pub fn own_process_group() -> u32 {
    // SAFETY: getpgrp has no arguments and only reads process identity.
    (unsafe { libc::getpgrp() }) as u32
}

/// See [`own_process_group`].
#[cfg(not(unix))]
#[must_use]
pub fn own_process_group() -> u32 {
    std::process::id()
}

/// Whether `pid` is a live process (`kill(pid, 0)` succeeded). A process
/// the caller may not signal counts as live; only a definite ESRCH (or an
/// out-of-range pid) reads as gone.
#[cfg(unix)]
#[must_use]
pub fn pid_alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    // SAFETY: signal zero probes existence without delivering a signal;
    // `kill(2)` reads no caller memory.
    (unsafe { libc::kill(pid, 0) }) == 0
}

/// Non-POSIX platforms cannot probe liveness this way: report alive.
#[cfg(not(unix))]
#[must_use]
pub fn pid_alive(_pid: u32) -> bool {
    true
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    fn spawn_null(std_cmd: &mut Command) -> std::process::Child {
        std_cmd
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn test helper")
    }

    fn sleeper() -> std::process::Child {
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        spawn_in_new_group(&mut cmd);
        spawn_null(&mut cmd)
    }

    #[test]
    fn term_in_new_group_terminates_the_group() {
        let mut child = sleeper();
        let pid = child.id();
        let group = ProcessGroup::from_leader_pid(pid).expect("owned group");
        assert!(!group.is_gone(), "fresh sleep 30 must be live");
        let outcome = group.signal(Signal::Term).expect("TERM owned group");
        assert!(matches!(
            outcome,
            SignalOutcome::Delivered | SignalOutcome::AlreadyGone
        ));
        // Reap the leader first: an unreaped zombie still counts as a live
        // group member, so kernel absence is only observable after wait.
        child.wait().expect("reap sleep after TERM");
        assert!(group.is_gone(), "group survived SIGTERM");
    }

    #[test]
    fn signal_after_reap_reports_already_gone() {
        // `sleep 0.01` exits at once; after reaping, the one-member group
        // is gone and kill(2) answers ESRCH.
        let mut cmd = Command::new("/bin/sleep");
        cmd.arg("0.01");
        spawn_in_new_group(&mut cmd);
        let mut child = spawn_null(&mut cmd);
        let pid = child.id();
        child.wait().expect("reap sleep");
        let group = ProcessGroup::from_leader_pid(pid).expect("owned group");
        assert!(group.is_gone(), "reaped group must read gone");
        assert_eq!(
            group.signal(Signal::Kill).expect("ESRCH is not an error"),
            SignalOutcome::AlreadyGone
        );
    }

    #[test]
    fn refuses_privileged_self_and_own_groups() {
        for refused in [
            0,
            1,
            std::process::id(),
            own_process_group(),
            u32::try_from(i32::MAX).unwrap() + 1,
        ] {
            assert!(
                matches!(
                    ProcessGroup::from_leader_pid(refused),
                    Err(SysError::Refused { pgid }) if pgid == refused
                ),
                "must refuse {refused}"
            );
        }
    }

    #[test]
    fn pid_alive_sees_self_and_rejects_reaped_child() {
        assert!(pid_alive(std::process::id()));
        let mut cmd = Command::new("/bin/sleep");
        cmd.arg("0.01");
        let mut child = spawn_null(&mut cmd);
        let pid = child.id();
        child.wait().expect("reap sleep");
        assert!(!pid_alive(pid), "reaped child must read gone");
    }
}
