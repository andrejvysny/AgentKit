//! Process identity beyond the pid: a helper is "ours" only when its pid
//! AND its start time match the record, so a recycled pid is never killed.
//!
//! The start time is opaque: microseconds since the epoch on macOS
//! (`proc_pidinfo`), clock ticks since boot on Linux (`/proc/<pid>/stat`).
//! It is only ever compared with a value read the same way on the same
//! host.
//!
//! This is platform code with `unsafe` outside `agentkit-sys` (ADR 0021
//! R9); it belongs there and moves when that crate is next changed.

use std::io;

/// What a process lookup found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Lookup {
    /// No process has the pid (or it belongs to another user).
    Gone,
    /// A process has the pid.
    Found {
        /// When it started (see the module documentation).
        started: u64,
        /// It has exited and waits to be reaped.
        zombie: bool,
    },
}

/// Look up `pid`.
#[cfg(target_os = "macos")]
pub(crate) fn lookup(pid: u32) -> io::Result<Lookup> {
    let Ok(pid) = i32::try_from(pid) else {
        return Ok(Lookup::Gone);
    };
    if pid <= 0 {
        return Ok(Lookup::Gone);
    }
    // SAFETY: `proc_bsdinfo` is a plain C struct of integers and byte
    // arrays; all-zero is a valid value of it.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: `info` is a writable local of exactly `size` bytes, which is
    // what PROC_PIDTBSDINFO writes; the call reads no other caller memory.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            std::ptr::addr_of_mut!(info).cast(),
            size,
        )
    };
    if written == size {
        let started = info
            .pbi_start_tvsec
            .saturating_mul(1_000_000)
            .saturating_add(info.pbi_start_tvusec);
        return Ok(Lookup::Found {
            started,
            zombie: info.pbi_status == libc::SZOMB,
        });
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        // A zombie has no BSD info to read on macOS; it is dead either way.
        Some(libc::ESRCH) | Some(libc::EPERM) | Some(libc::EACCES) => Ok(Lookup::Gone),
        _ if written >= 0 => Err(io::Error::other("short proc_pidinfo answer")),
        _ => Err(error),
    }
}

/// Look up `pid` (`/proc/<pid>/stat`: state is field 3, start time field
/// 22, both after the parenthesized command name).
#[cfg(target_os = "linux")]
pub(crate) fn lookup(pid: u32) -> io::Result<Lookup> {
    if pid == 0 {
        return Ok(Lookup::Gone);
    }
    let text = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Lookup::Gone),
        Err(error) => return Err(error),
    };
    let rest = text
        .rsplit_once(')')
        .map(|(_, rest)| rest)
        .ok_or_else(|| io::Error::other("unreadable /proc stat"))?;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let state = fields.first().copied().unwrap_or("");
    let started = fields
        .get(19)
        .and_then(|field| field.parse::<u64>().ok())
        .ok_or_else(|| io::Error::other("unreadable /proc start time"))?;
    Ok(Lookup::Found {
        started,
        zombie: state == "Z" || state == "X",
    })
}

/// No way to tell one process from another here.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub(crate) fn lookup(_pid: u32) -> io::Result<Lookup> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "process start times are not available on this platform",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_process_has_a_stable_start_time_and_pid_zero_is_gone() {
        let pid = std::process::id();
        let first = lookup(pid).unwrap();
        let Lookup::Found { started, zombie } = first else {
            panic!("own pid not found: {first:?}");
        };
        assert!(started > 0 && !zombie);
        assert_eq!(lookup(pid).unwrap(), first);
        assert_eq!(lookup(0).unwrap(), Lookup::Gone);
    }

    #[test]
    fn a_child_differs_from_its_parent_until_it_is_reaped() {
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = child.id();
        assert!(matches!(
            lookup(pid).unwrap(),
            Lookup::Found { zombie: false, .. }
        ));
        child.kill().unwrap();
        child.wait().unwrap();
        // Reaped: gone, unless the pid was reused already, which recovery
        // tells apart by the start time.
        if let Lookup::Found { zombie, .. } = lookup(pid).unwrap() {
            assert!(!zombie, "a reaped child is no zombie");
        }
    }
}
