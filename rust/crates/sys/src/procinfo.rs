//! Process identity and open files, for supervisors that must tell one
//! process from another and prove that nothing still uses a file.
//!
//! - [`lookup_process`]: whether a pid exists, when it started and whether
//!   it is a zombie. A pid names "the same process" only when its start
//!   time matches too, so a recycled pid is never taken for a recorded
//!   one. The start time is opaque: microseconds since the epoch on macOS
//!   (`proc_pidinfo`), clock ticks since boot on Linux
//!   (`/proc/<pid>/stat`). Compare it only with a value read the same way
//!   on the same host.
//! - [`pids_holding`]: the processes that have a descriptor on a file,
//!   matched by device and inode. On macOS every process's descriptors are
//!   read from `proc_pidinfo` (`procinfo/libproc.rs`; descriptors opened only for
//!   event notification are left out), on Linux from `/proc/<pid>/fd`.
//!   Descriptors only: a working directory or a mapping without a
//!   descriptor is not seen. Both see only the processes the caller may
//!   inspect: its own user's, unless it is root.
//!
//! Other platforms refuse with [`std::io::ErrorKind::Unsupported`].

use std::io;
use std::path::Path;

/// What [`lookup_process`] found for one pid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessLookup {
    /// No process has the pid, or it belongs to a user the caller may not
    /// inspect.
    Gone,
    /// A process has the pid.
    Found {
        /// When it started (see the module documentation).
        started: u64,
        /// It has exited and waits to be reaped.
        zombie: bool,
    },
}

/// Look up `pid` (`proc_pidinfo(PROC_PIDTBSDINFO)`).
#[cfg(target_os = "macos")]
pub fn lookup_process(pid: u32) -> io::Result<ProcessLookup> {
    let Ok(pid) = i32::try_from(pid) else {
        return Ok(ProcessLookup::Gone);
    };
    if pid <= 0 {
        return Ok(ProcessLookup::Gone);
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
        return Ok(ProcessLookup::Found {
            started,
            zombie: info.pbi_status == libc::SZOMB,
        });
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        // A zombie has no BSD info to read on macOS; it is dead either way.
        Some(libc::ESRCH) | Some(libc::EPERM) | Some(libc::EACCES) => Ok(ProcessLookup::Gone),
        _ if written >= 0 => Err(io::Error::other("short proc_pidinfo answer")),
        _ => Err(error),
    }
}

/// Look up `pid` (`/proc/<pid>/stat`: state is field 3, start time field
/// 22, both after the parenthesized command name).
#[cfg(target_os = "linux")]
pub fn lookup_process(pid: u32) -> io::Result<ProcessLookup> {
    if pid == 0 {
        return Ok(ProcessLookup::Gone);
    }
    let text = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(ProcessLookup::Gone),
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
    Ok(ProcessLookup::Found {
        started,
        zombie: state == "Z" || state == "X",
    })
}

/// No way to tell one process from another here.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn lookup_process(_pid: u32) -> io::Result<ProcessLookup> {
    Err(unsupported())
}

/// The pids of up to `max` processes that have `path` open, the caller
/// included. A path that does not exist has no holders.
#[cfg(target_os = "macos")]
pub fn pids_holding(path: &Path, max: usize) -> io::Result<Vec<u32>> {
    use std::os::unix::fs::MetadataExt;
    let wanted = match std::fs::metadata(path) {
        // The kernel names a vnode's device in 32 bits; std widens it.
        Ok(meta) => libproc::FileId {
            dev: meta.dev() as u32,
            ino: meta.ino(),
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let max = max.clamp(1, PIDS_MAX);
    let mut holders = Vec::new();
    for pid in libproc::all_pids()? {
        if libproc::holds(pid, wanted)? {
            holders.extend(u32::try_from(pid).ok());
            if holders.len() >= max {
                break;
            }
        }
    }
    Ok(holders)
}

/// The pids of up to `max` processes that have `path` open, the caller
/// included. A path that does not exist has no holders.
#[cfg(target_os = "linux")]
pub fn pids_holding(path: &Path, max: usize) -> io::Result<Vec<u32>> {
    holders_in(Path::new("/proc"), path, max.clamp(1, PIDS_MAX))
}

/// See [`pids_holding`].
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn pids_holding(_path: &Path, _max: usize) -> io::Result<Vec<u32>> {
    Err(unsupported())
}

/// Most pids one [`pids_holding`] call answers.
#[cfg(any(target_os = "macos", target_os = "linux"))]
const PIDS_MAX: usize = 4096;

/// The processes below `proc_root` (a `/proc` layout) with a descriptor
/// on the file `target` names: same device and inode, which is what each
/// `fd/<n>` link resolves to. A process that ends meanwhile, or whose
/// descriptors the caller may not read, is skipped.
#[cfg(unix)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn holders_in(proc_root: &Path, target: &Path, max: usize) -> io::Result<Vec<u32>> {
    use std::os::unix::fs::MetadataExt;
    let wanted = match std::fs::metadata(target) {
        Ok(meta) => (meta.dev(), meta.ino()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let is_target =
        |fd: &Path| std::fs::metadata(fd).is_ok_and(|meta| (meta.dev(), meta.ino()) == wanted);
    let mut pids = Vec::new();
    for entry in std::fs::read_dir(proc_root)?.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(fds) = std::fs::read_dir(entry.path().join("fd")) else {
            continue;
        };
        if fds.flatten().any(|fd| is_target(&fd.path())) {
            pids.push(pid);
            if pids.len() >= max {
                break;
            }
        }
    }
    Ok(pids)
}

#[cfg(target_os = "macos")]
mod libproc;

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn unsupported() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "process start times and open-file holders are not available on this platform",
    )
}

#[cfg(all(test, unix))]
mod tests;
