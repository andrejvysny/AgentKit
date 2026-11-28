//! Init-process primitives for a sandbox supervisor (ADR 0039): the guest
//! supervisor runs as PID 1 of its container, starts each command in its
//! own session, reaps every child the kernel hands it, and ends every
//! process but itself when an operation ends.
//!
//! - `prctl(2)` on Linux: [`set_non_dumpable`] and [`set_child_subreaper`].
//! - `setsid` + `fchdir` in the child before exec: [`spawn_in_new_session`],
//!   so the working directory is the descriptor the caller resolved, never
//!   a path looked up again.
//! - `kill(-1, sig)`: [`kill_all_processes`], refused unless the caller is
//!   PID 1 (outside a pid namespace it would signal every process of the
//!   user).
//! - `waitpid(2)` with `WNOHANG`: [`reap_any`] and [`reap_child`].
//! - Every process below a pid, from `/proc` (Linux): [`descendants`].
//! - `sync(2)`: [`sync_filesystems`].
//! - `RLIMIT_NOFILE` (macOS, Linux): [`raise_open_file_limit`], for a process
//!   that bridges many connections at once.
//!
//! Linux implements the `prctl` and `/proc` parts; other Unix platforms
//! refuse them with [`std::io::ErrorKind::Unsupported`] and implement the
//! rest. Non-Unix platforms refuse everything.

use std::fs::File;
use std::io;

use crate::process::{ChildExit, Signal, SignalOutcome};

/// What one [`reap_any`] call found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reaped {
    /// A child ended and was reaped.
    Child {
        /// Its pid.
        pid: u32,
        /// How it ended.
        exit: ChildExit,
    },
    /// Children exist but none has ended yet.
    NoneReady,
    /// The caller has no children at all (`ECHILD`).
    NoChildren,
}

/// True when the caller is PID 1 of its pid namespace.
#[must_use]
pub fn is_init() -> bool {
    std::process::id() == 1
}

/// Mark the process non-dumpable (`PR_SET_DUMPABLE 0`): no core dumps and
/// no `ptrace` attach or `/proc/<pid>/mem` access by same-uid processes.
#[cfg(target_os = "linux")]
pub fn set_non_dumpable() -> io::Result<()> {
    let zero: libc::c_ulong = 0;
    // SAFETY: `PR_SET_DUMPABLE` reads its integer arguments only and
    // touches no caller memory.
    let rc = unsafe { libc::prctl(libc::PR_SET_DUMPABLE, zero, zero, zero, zero) };
    crate::fs::check(rc)
}

/// See [`set_non_dumpable`]; only Linux has `prctl(2)`.
#[cfg(not(target_os = "linux"))]
pub fn set_non_dumpable() -> io::Result<()> {
    Err(unsupported())
}

/// Become a child subreaper (`PR_SET_CHILD_SUBREAPER 1`): orphaned
/// descendants are reparented to the caller instead of to init, so the
/// caller can still see, kill and reap them.
#[cfg(target_os = "linux")]
pub fn set_child_subreaper() -> io::Result<()> {
    let (one, zero): (libc::c_ulong, libc::c_ulong) = (1, 0);
    // SAFETY: `PR_SET_CHILD_SUBREAPER` reads its integer arguments only and
    // touches no caller memory.
    let rc = unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, one, zero, zero, zero) };
    crate::fs::check(rc)
}

/// See [`set_child_subreaper`]; only Linux has `prctl(2)`.
#[cfg(not(target_os = "linux"))]
pub fn set_child_subreaper() -> io::Result<()> {
    Err(unsupported())
}

/// `kill(-1, sig)`: signal every process the caller may signal except
/// itself. Refused with `PermissionDenied` unless the caller is PID 1, where
/// "every process" is exactly its own pid namespace. `ESRCH` (nothing left
/// to signal) is [`SignalOutcome::AlreadyGone`].
#[cfg(unix)]
pub fn kill_all_processes(sig: Signal) -> io::Result<SignalOutcome> {
    if !is_init() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "kill(-1) is reserved to PID 1",
        ));
    }
    // SAFETY: only PID 1 gets here, where -1 addresses every process of
    // its own pid namespace except the caller; `kill(2)` reads no memory.
    if unsafe { libc::kill(-1, sig.number()) } == 0 {
        return Ok(SignalOutcome::Delivered);
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        return Ok(SignalOutcome::AlreadyGone);
    }
    Err(error)
}

/// See [`kill_all_processes`].
#[cfg(not(unix))]
pub fn kill_all_processes(_sig: Signal) -> io::Result<SignalOutcome> {
    Err(unsupported())
}

/// Classify a `waitpid(2)` status word.
#[cfg(unix)]
fn decode_status(status: libc::c_int) -> ChildExit {
    if libc::WIFEXITED(status) {
        ChildExit::Exited {
            code: libc::WEXITSTATUS(status),
        }
    } else if libc::WIFSIGNALED(status) && libc::WCOREDUMP(status) {
        ChildExit::Dumped {
            signal: libc::WTERMSIG(status),
        }
    } else if libc::WIFSIGNALED(status) {
        ChildExit::Killed {
            signal: libc::WTERMSIG(status),
        }
    } else {
        ChildExit::Running
    }
}

/// Reap one ended child of the caller, whichever it is
/// (`waitpid(-1, WNOHANG)`). Never blocks; `EINTR` is retried.
#[cfg(unix)]
pub fn reap_any() -> io::Result<Reaped> {
    loop {
        let mut status: libc::c_int = 0;
        // SAFETY: `status` is a writable local owned by this call; -1 with
        // WNOHANG only collects an ended child of the caller and never
        // blocks.
        let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
        if pid > 0 {
            let pid = u32::try_from(pid).unwrap_or(0);
            let exit = decode_status(status);
            return Ok(Reaped::Child { pid, exit });
        }
        if pid == 0 {
            return Ok(Reaped::NoneReady);
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(libc::ECHILD) => return Ok(Reaped::NoChildren),
            _ => return Err(error),
        }
    }
}

/// See [`reap_any`].
#[cfg(not(unix))]
pub fn reap_any() -> io::Result<Reaped> {
    Err(unsupported())
}

/// Reap the child `pid` if it has ended (`waitpid(pid, WNOHANG)`):
/// [`ChildExit::Running`] when it has not, [`ChildExit::NoSuchChild`] when
/// it is not (or no longer) the caller's child.
#[cfg(unix)]
pub fn reap_child(pid: u32) -> io::Result<ChildExit> {
    let Ok(target) = i32::try_from(pid) else {
        return Ok(ChildExit::NoSuchChild);
    };
    if target <= 0 {
        return Ok(ChildExit::NoSuchChild);
    }
    loop {
        let mut status: libc::c_int = 0;
        // SAFETY: `status` is a writable local owned by this call; a
        // positive pid with WNOHANG only collects that child and never
        // blocks.
        let rc = unsafe { libc::waitpid(target, &mut status, libc::WNOHANG) };
        if rc > 0 {
            return Ok(decode_status(status));
        }
        if rc == 0 {
            return Ok(ChildExit::Running);
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(libc::ECHILD) => return Ok(ChildExit::NoSuchChild),
            _ => return Err(error),
        }
    }
}

/// See [`reap_child`].
#[cfg(not(unix))]
pub fn reap_child(_pid: u32) -> io::Result<ChildExit> {
    Err(unsupported())
}

/// Start the child in a new session (`setsid`: its own session and
/// process group, no controlling terminal) with `cwd` as its working
/// directory (`fchdir` on the open descriptor, so a path component
/// swapped for a link after the caller resolved it cannot redirect it).
/// `cwd` stays open until `cmd` is dropped; it is close-on-exec, so the
/// command never inherits it.
#[cfg(unix)]
pub fn spawn_in_new_session(cmd: &mut std::process::Command, cwd: File) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;
    let hook = move || -> io::Result<()> {
        let dir = cwd.as_raw_fd();
        // SAFETY: runs in the forked child before exec; `setsid(2)` is
        // async-signal-safe, takes no arguments and touches no memory.
        if unsafe { libc::setsid() } == -1 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: async-signal-safe; `dir` is the caller's descriptor,
        // still open in the child because `cwd` lives in this closure.
        if unsafe { libc::fchdir(dir) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    };
    // SAFETY: the hook only makes the async-signal-safe calls above; it
    // allocates nothing and takes no lock, as `pre_exec` requires.
    unsafe { cmd.pre_exec(hook) };
    Ok(())
}

/// See [`spawn_in_new_session`].
#[cfg(not(unix))]
pub fn spawn_in_new_session(_cmd: &mut std::process::Command, _cwd: File) -> io::Result<()> {
    Err(unsupported())
}

/// Raise the soft limit of open descriptors (`RLIMIT_NOFILE`) to `want`,
/// or to the hard limit when that is lower; never lowers it. Answers the
/// soft limit in force afterwards.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub fn raise_open_file_limit(want: u64) -> io::Result<u64> {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `limit` is a writable local of the type `getrlimit` fills.
    crate::fs::check(unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) })?;
    let target = want.min(limit.rlim_max);
    if limit.rlim_cur >= target {
        return Ok(limit.rlim_cur);
    }
    limit.rlim_cur = target;
    // SAFETY: `setrlimit` only reads `limit`, an initialised value.
    crate::fs::check(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) })?;
    Ok(target)
}

/// See [`raise_open_file_limit`].
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn raise_open_file_limit(_want: u64) -> io::Result<u64> {
    Err(unsupported())
}

/// `sync(2)`: schedule every dirty buffer of every filesystem for
/// writing. No-op where there is no `sync(2)`.
pub fn sync_filesystems() {
    #[cfg(unix)]
    // SAFETY: `sync(2)` takes no arguments, touches no caller memory and
    // cannot fail.
    unsafe {
        libc::sync();
    }
}

/// Every process below `root` (children, grandchildren, ...), from the
/// parent links in `/proc/<pid>/stat`. A snapshot: processes may fork or
/// end while it is taken, so callers loop until it is empty.
#[cfg(target_os = "linux")]
pub fn descendants(root: u32) -> io::Result<Vec<u32>> {
    let mut links = Vec::new();
    for entry in std::fs::read_dir("/proc")? {
        let Ok(entry) = entry else { continue };
        let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse().ok()) else {
            continue;
        };
        // A process may end between the listing and the read.
        let Ok(text) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        if let Some(ppid) = parse_stat_ppid(&text) {
            links.push((pid, ppid));
        }
    }
    Ok(below(root, &links))
}

/// See [`descendants`]; only Linux has `/proc`.
#[cfg(not(target_os = "linux"))]
pub fn descendants(_root: u32) -> io::Result<Vec<u32>> {
    Err(unsupported())
}

/// The parent pid in the text of `/proc/<pid>/stat`: `pid (comm) state
/// ppid ...`, where `comm` may itself hold spaces and parentheses.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_stat_ppid(text: &str) -> Option<u32> {
    let rest = &text[text.rfind(')')? + 1..];
    let mut fields = rest.split_whitespace();
    fields.next()?;
    fields.next()?.parse().ok()
}

/// Every pid whose parent chain reaches `root`, `root` excluded.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn below(root: u32, links: &[(u32, u32)]) -> Vec<u32> {
    let mut found = Vec::new();
    let mut frontier = vec![root];
    while let Some(parent) = frontier.pop() {
        for &(pid, ppid) in links {
            if ppid == parent && pid != root && !found.contains(&pid) {
                found.push(pid);
                frontier.push(pid);
            }
        }
    }
    found
}

#[cfg(not(target_os = "linux"))]
fn unsupported() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "this init-process primitive is unsupported on this platform",
    )
}

#[cfg(all(test, unix))]
mod tests;
