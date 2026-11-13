//! OS process-group primitives.
//!
//! A [`ProcessGroup`] is a handle to a child process group the caller
//! spawned with `spawn_in_new_group*` (pgid == child pid). Construction
//! refuses privileged and self targets, mirroring the refusal rules the
//! claude transport enforced at every kill since AGENTKIT-110: no group
//! `0`/`1`, no group above `i32::MAX`, never the caller's own pid and
//! never the caller's own process group. A signal to a group that is
//! already gone is a success [`SignalOutcome::AlreadyGone`], not an error.
//!
//! Single-pid supervision rides the same discipline: [`signal_process`]
//! refuses privileged and self pids, [`kill_probe`] exposes `kill(2)`
//! error-checking (pid, group and own-group addressing),
//! [`peek_child_exit`] reports a child's exit without reaping it, and
//! [`own_parent_pid`] / [`effective_uid`] answer identity questions.

use std::io;

/// Errors from the process and file primitives. Per ADR 0021 R7 the
/// discriminant is the outcome class; strings are context only.
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
    /// The caller asked to signal a pid it must not own (privileged or
    /// self pid).
    #[error("refusing to signal pid {pid}")]
    RefusedPid {
        /// The rejected pid.
        pid: u32,
    },
    /// `flock(2)` failed for a reason other than lock contention
    /// (contention is `try_lock_exclusive`'s `Ok(false)`, not an error).
    #[error("flock({operation}) failed: {source}")]
    Flock {
        /// The attempted operation: "lock", "try-lock" or "unlock".
        operation: &'static str,
        /// The OS error `flock(2)` reported.
        source: io::Error,
    },
    /// The platform does not implement POSIX process groups.
    #[error("process-group control is unsupported on this platform")]
    Unsupported,
}

/// Signals the supervisor layer sends to owned process groups and pids.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Signal {
    /// Interrupt (ladder rung 1).
    Int,
    /// Terminate (ladder rung 2).
    Term,
    /// Kill (ladder rung 3; unrecoverable).
    Kill,
    /// Stop (freeze before the kill so the target cannot fork).
    Stop,
    /// Continue (a stopped target only sees earlier signals once continued).
    Cont,
}

#[cfg(unix)]
impl Signal {
    /// The raw signal number `kill(2)` receives for this signal.
    #[must_use]
    pub fn number(self) -> i32 {
        match self {
            Signal::Int => libc::SIGINT,
            Signal::Term => libc::SIGTERM,
            Signal::Kill => libc::SIGKILL,
            Signal::Stop => libc::SIGSTOP,
            Signal::Cont => libc::SIGCONT,
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

/// Send `sig` to ONE process (`kill(2)` with a positive pid), for
/// supervisors that signal individually proven pids instead of a group.
/// Same refusal discipline as [`ProcessGroup::from_leader_pid`]: privileged
/// ids (`0`, `1`), ids above `i32::MAX` and the caller's own pid are
/// refused, so a corrupted pid can never turn a kill into a broadcast,
/// init- or self-kill. ESRCH is [`SignalOutcome::AlreadyGone`], not an
/// error.
#[cfg(unix)]
pub fn signal_process(pid: u32, sig: Signal) -> Result<SignalOutcome, SysError> {
    if pid <= 1 || pid > i32::MAX as u32 || pid == std::process::id() {
        return Err(SysError::RefusedPid { pid });
    }
    // SAFETY: `pid` was validated above — a positive id that is neither
    // our own nor privileged — and `kill(2)` reads no caller memory.
    let rc = unsafe { libc::kill(pid as i32, sig.number()) };
    if rc == 0 {
        return Ok(SignalOutcome::Delivered);
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        return Ok(SignalOutcome::AlreadyGone);
    }
    Err(SysError::Signal {
        pgid: pid,
        signal: sig.number(),
        source: error,
    })
}

/// See [`signal_process`]; non-POSIX platforms have no signals to send.
#[cfg(not(unix))]
pub fn signal_process(_pid: u32, _sig: Signal) -> Result<SignalOutcome, SysError> {
    Err(SysError::Unsupported)
}

/// Outcome of `kill(target, 0)`: the error-checking signal probe that
/// delivers nothing. `target` follows `kill(2)` addressing verbatim —
/// positive = one pid, `0` = the caller's process group, negative = the
/// group `|target|` — so callers reproduce raw `kill(pid, 0)` semantics
/// without touching `libc`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KillProbe {
    /// At least one target exists and accepts signals from us.
    Alive,
    /// ESRCH: every target is gone (or the id is out of `pid_t` range).
    Gone,
    /// The target exists but belongs to someone else (EPERM and friends):
    /// it is there, we just may not touch it.
    NotPermitted,
}

/// Probe `target` with `kill(target, 0)`, preserving `kill(2)` addressing
/// for pids, groups (`target < 0`) and the caller's own group (`0`).
#[cfg(unix)]
#[must_use]
pub fn kill_probe(target: i32) -> KillProbe {
    // SAFETY: signal zero performs error checking only and delivers no
    // signal; `kill(2)` reads no caller memory.
    if unsafe { libc::kill(target, 0) } == 0 {
        return KillProbe::Alive;
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        KillProbe::Gone
    } else {
        KillProbe::NotPermitted
    }
}

/// No POSIX signals to probe with; a false "gone" would retire leases on
/// live work, so non-POSIX platforms report alive.
#[cfg(not(unix))]
#[must_use]
pub fn kill_probe(_target: i32) -> KillProbe {
    KillProbe::Alive
}

/// What [`peek_child_exit`] observed about one of the caller's children,
/// WITHOUT reaping it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChildExit {
    /// The child exited with this code.
    Exited {
        /// The exit code (`si_status` for `CLD_EXITED`).
        code: i32,
    },
    /// The child was killed by this signal.
    Killed {
        /// The terminating signal (`si_status` for `CLD_KILLED`).
        signal: i32,
    },
    /// The child was killed by this signal and dumped core.
    Dumped {
        /// The terminating signal (`si_status` for `CLD_DUMPED`).
        signal: i32,
    },
    /// No state change yet (`WNOHANG` answered success with no report, or
    /// the child only stopped/continued); the caller should poll again.
    Running,
    /// ECHILD: no such child (already reaped, or never ours).
    NoSuchChild,
}

/// Non-reaping exit-status probe: `waitid(P_PID, pid, WNOWAIT | WEXITED |
/// WNOHANG)`. Reports the child's exit while LEAVING it a zombie, so a
/// supervisor keeps the un-reaped child — and with it the unrecyclable
/// process-group number — until teardown has finished signalling.
/// `WNOHANG` is load-bearing: without it `waitid` BLOCKS until the child
/// changes state. An `Err` carries the transient `errno` (e.g. EINTR) for
/// callers that keep polling; out-of-range pids read as
/// [`ChildExit::NoSuchChild`] rather than wrapping into `pid_t`.
#[cfg(unix)]
pub fn peek_child_exit(pid: u32) -> Result<ChildExit, io::Error> {
    if pid == 0 || pid > i32::MAX as u32 {
        return Ok(ChildExit::NoSuchChild);
    }
    let mut info: libc::siginfo_t = unsafe {
        // SAFETY: `siginfo_t` is a plain C data struct; zeroing it is
        // what every `waitid(2)` caller does before the call, and the
        // kernel fully writes it before we read `si_code`/`si_status`.
        std::mem::zeroed()
    };
    // SAFETY: `info` is a writable local owned by this call; `pid` is in
    // `pid_t` range; WNOWAIT|WEXITED|WNOHANG only reports our own
    // un-reaped child's status, never reaps it, and never blocks.
    let rc = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            std::ptr::addr_of_mut!(info),
            libc::WNOWAIT | libc::WEXITED | libc::WNOHANG,
        )
    };
    if rc != 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ECHILD) {
            return Ok(ChildExit::NoSuchChild);
        }
        return Err(error);
    }
    let code = info.si_code;
    if !matches!(code, libc::CLD_EXITED | libc::CLD_KILLED | libc::CLD_DUMPED) {
        // WNOHANG with no exit yet. Also covers XNU, which reports
        // CLD_STOPPED / CLD_CONTINUED despite the WEXITED-only filter;
        // reading them as an exit would settle a live child.
        return Ok(ChildExit::Running);
    }
    // SAFETY: on success waitid filled si_code with the exit
    // classification and si_status with the raw exit code or terminating
    // signal of this child.
    let status = unsafe { info.si_status() };
    Ok(match code {
        libc::CLD_EXITED => ChildExit::Exited { code: status },
        libc::CLD_KILLED => ChildExit::Killed { signal: status },
        _ => ChildExit::Dumped { signal: status },
    })
}

/// See [`peek_child_exit`]; platforms without `waitid(2)` refuse.
#[cfg(not(unix))]
pub fn peek_child_exit(_pid: u32) -> Result<ChildExit, io::Error> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "child exit observation is unsupported on this platform",
    ))
}

/// The caller's parent pid (`getppid(2)`), for supervisors that must
/// refuse their own parent as a kill target. Non-POSIX platforms have no
/// `pid_t` parent to report and refuse with `Unsupported`.
#[cfg(unix)]
pub fn own_parent_pid() -> Result<u32, SysError> {
    // SAFETY: getppid has no arguments, touches no memory and cannot fail.
    Ok(unsafe { libc::getppid() } as u32)
}

/// See [`own_parent_pid`].
#[cfg(not(unix))]
pub fn own_parent_pid() -> Result<u32, SysError> {
    Err(SysError::Unsupported)
}

/// The caller's effective uid (`geteuid(2)`), for naming and verifying
/// per-user private state directories. Non-POSIX platforms have no POSIX
/// uid and refuse with `Unsupported`.
#[cfg(unix)]
pub fn effective_uid() -> Result<u32, SysError> {
    // SAFETY: geteuid has no arguments, touches no memory and cannot fail.
    Ok(unsafe { libc::geteuid() })
}

/// See [`effective_uid`].
#[cfg(not(unix))]
pub fn effective_uid() -> Result<u32, SysError> {
    Err(SysError::Unsupported)
}

/// The caller's REAL uid (`getuid(2)`) — the id Node's `process.getuid()`
/// reports, which the lock-directory coordinator name keys on; verify
/// ownership with [`effective_uid`] instead. Non-POSIX platforms have no
/// POSIX uid and refuse with `Unsupported`.
#[cfg(unix)]
pub fn real_uid() -> Result<u32, SysError> {
    // SAFETY: getuid has no arguments, touches no memory and cannot fail.
    Ok(unsafe { libc::getuid() })
}

/// See [`real_uid`].
#[cfg(not(unix))]
pub fn real_uid() -> Result<u32, SysError> {
    Err(SysError::Unsupported)
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

    #[test]
    fn signal_numbers_match_the_platform() {
        assert_eq!(Signal::Int.number(), libc::SIGINT);
        assert_eq!(Signal::Term.number(), libc::SIGTERM);
        assert_eq!(Signal::Kill.number(), libc::SIGKILL);
        assert_eq!(Signal::Stop.number(), libc::SIGSTOP);
        assert_eq!(Signal::Cont.number(), libc::SIGCONT);
    }

    #[test]
    fn signal_process_delivers_then_reports_already_gone() {
        let mut child = sleeper();
        let pid = child.id();
        assert_eq!(
            signal_process(pid, Signal::Term).expect("TERM owned child"),
            SignalOutcome::Delivered
        );
        child.wait().expect("reap sleep after TERM");
        assert_eq!(
            signal_process(pid, Signal::Kill).expect("ESRCH is not an error"),
            SignalOutcome::AlreadyGone
        );
    }

    #[test]
    fn signal_process_refuses_privileged_and_self_pids() {
        for refused in [
            0,
            1,
            std::process::id(),
            u32::try_from(i32::MAX).unwrap() + 1,
        ] {
            assert!(
                matches!(
                    signal_process(refused, Signal::Kill),
                    Err(SysError::RefusedPid { pid }) if pid == refused
                ),
                "must refuse pid {refused}"
            );
        }
    }

    #[test]
    fn kill_probe_answers_alive_gone_and_own_group() {
        assert_eq!(kill_probe(std::process::id() as i32), KillProbe::Alive);
        // `0` addresses the caller's own process group, which is live by
        // definition, mirroring `kill(0, 0)` returning 0.
        assert_eq!(kill_probe(0), KillProbe::Alive);
        let mut cmd = Command::new("/bin/sleep");
        cmd.arg("0.01");
        let mut child = spawn_null(&mut cmd);
        let pid = i32::try_from(child.id()).expect("test pid fits i32");
        assert_eq!(kill_probe(pid), KillProbe::Alive);
        child.wait().expect("reap sleep");
        assert_eq!(kill_probe(pid), KillProbe::Gone);
    }

    #[test]
    fn peek_child_exit_reports_exit_without_reaping_then_no_child() {
        let mut cmd = Command::new("/bin/sleep");
        cmd.arg("0.01");
        let mut child = spawn_null(&mut cmd);
        let pid = child.id();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            match peek_child_exit(pid).expect("peek our own child") {
                ChildExit::Exited { code: 0 } => break,
                ChildExit::Running => {
                    assert!(std::time::Instant::now() < deadline, "child never exited");
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                other => panic!("unexpected observation: {other:?}"),
            }
        }
        // WNOWAIT held the zombie back: still a live kill target, so the
        // caller's group number stays unrecyclable until it reaps itself.
        let pid_i32 = i32::try_from(pid).expect("test pid fits i32");
        assert_eq!(kill_probe(pid_i32), KillProbe::Alive);
        child.wait().expect("reap sleep");
        assert!(matches!(peek_child_exit(pid), Ok(ChildExit::NoSuchChild)));
    }

    #[test]
    fn peek_child_exit_refuses_pids_that_are_not_our_children() {
        assert!(matches!(peek_child_exit(0), Ok(ChildExit::NoSuchChild)));
        assert!(matches!(
            peek_child_exit(u32::try_from(i32::MAX).unwrap() + 1),
            Ok(ChildExit::NoSuchChild)
        ));
        // Our own pid is not our child: waitid answers ECHILD.
        assert!(matches!(
            peek_child_exit(std::process::id()),
            Ok(ChildExit::NoSuchChild)
        ));
    }

    #[test]
    fn real_uid_reports_getuid_not_geteuid() {
        // SAFETY: getuid has no arguments, touches no memory and cannot
        // fail.
        let expected = unsafe { libc::getuid() };
        assert_eq!(real_uid().expect("unix has a uid"), expected);
    }

    #[test]
    fn own_parent_pid_and_effective_uid_match_the_platform() {
        use std::os::unix::fs::MetadataExt;
        let parent = own_parent_pid().expect("unix has a parent pid");
        assert_ne!(parent, std::process::id());
        assert!(pid_alive(parent), "our parent must be live");
        let dir_owner = std::fs::metadata(std::env::current_dir().expect("cwd"))
            .expect("stat cwd")
            .uid();
        assert_eq!(effective_uid().expect("unix has an euid"), dir_owner);
    }
}
