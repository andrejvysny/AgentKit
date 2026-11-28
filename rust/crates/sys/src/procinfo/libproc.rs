//! The macOS descriptor scan behind [`super::pids_holding`]: every process
//! (`proc_listpids`), its descriptors (`PROC_PIDLISTFDS`) and the device
//! and inode of each vnode one (`PROC_PIDFDVNODEINFO`).
//!
//! `proc_listpidspath` is not used: it drops a whole process from its
//! answer when any one of that process's descriptors closes during the
//! scan, so a holder that opens and closes other files is missed. Here a
//! descriptor closed since the listing, or one the kernel will not describe
//! to the caller, is skipped alone; a process that is gone, or that the
//! caller may not inspect, holds nothing it can see; any other failure is
//! an error, never "not held".
//!
//! `proc_pidinfo` and `proc_pidfdinfo` answer 0 both for an empty answer
//! and for a failure (libproc turns the system call's -1 into 0), so errno
//! is cleared before each call and read when the answer is short.

use std::io;
use std::mem::{offset_of, size_of, size_of_val};

/// A file as the kernel names it in `vinfo_stat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct FileId {
    /// The device, 32 bits wide in `vinfo_stat`.
    pub(super) dev: u32,
    pub(super) ino: u64,
}

/// `PROC_ALL_PIDS` (`sys/proc_info.h`).
const PROC_ALL_PIDS: u32 = 1;
/// `PROC_PIDFDVNODEINFO` (`sys/proc_info.h`).
const PROC_PIDFDVNODEINFO: libc::c_int = 1;
/// Longest pid or descriptor list asked for.
const LIST_MAX: usize = 1 << 20;

/// `struct proc_fileinfo` (`sys/proc_info.h`); the `libc` crate lacks it.
#[repr(C)]
struct ProcFileinfo {
    fi_openflags: u32,
    fi_status: u32,
    fi_offset: libc::off_t,
    fi_type: i32,
    fi_guardflags: u32,
}

/// `struct vnode_fdinfo` (`sys/proc_info.h`); the `libc` crate lacks it.
#[repr(C)]
struct VnodeFdinfo {
    pfi: ProcFileinfo,
    pvi: libc::vnode_info,
}

// The layout of the macOS SDK's `sys/proc_info.h`, measured with clang.
const _: () = {
    assert!(size_of::<VnodeFdinfo>() == 176);
    assert!(offset_of!(VnodeFdinfo, pvi) == 24);
    assert!(offset_of!(VnodeFdinfo, pvi.vi_stat.vst_dev) == 24);
    assert!(offset_of!(VnodeFdinfo, pvi.vi_stat.vst_ino) == 32);
};
const VNODE_FDINFO_SIZE: libc::c_int = size_of::<VnodeFdinfo>() as libc::c_int;

/// Every pid, from a list grown until it fit.
pub(super) fn all_pids() -> io::Result<Vec<libc::pid_t>> {
    let mut room = 1024;
    loop {
        let mut pids: Vec<libc::pid_t> = vec![0; room];
        let bytes = buffer_bytes(&pids)?;
        // SAFETY: `pids` is a writable buffer of exactly `bytes` bytes, the
        // most the call writes.
        let written =
            unsafe { libc::proc_listpids(PROC_ALL_PIDS, 0, pids.as_mut_ptr().cast(), bytes) };
        let written = usize::try_from(written).map_err(|_| io::Error::last_os_error())?;
        let count = written / size_of::<libc::pid_t>();
        if count < room {
            pids.truncate(count);
            pids.retain(|&pid| pid > 0);
            return Ok(pids);
        }
        room = grow(room)?;
    }
}

/// True when `pid` has a descriptor on `wanted` that was not opened for
/// event notification only (`O_EVTONLY`, which cannot write the file).
pub(super) fn holds(pid: libc::pid_t, wanted: FileId) -> io::Result<bool> {
    let Some(fds) = descriptors(pid)? else {
        return Ok(false);
    };
    let vnodes = fds
        .iter()
        .filter(|fd| fd.proc_fdtype == libc::PROX_FDTYPE_VNODE as u32);
    for fd in vnodes {
        let Some(info) = vnode(pid, fd.proc_fd)? else {
            continue;
        };
        let stat = &info.pvi.vi_stat;
        let evtonly = info.pfi.fi_openflags & libc::O_EVTONLY as u32 != 0;
        if !evtonly && (stat.vst_dev, stat.vst_ino) == (wanted.dev, wanted.ino) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The descriptors of `pid`; `None` when it is gone or not the caller's to
/// inspect.
fn descriptors(pid: libc::pid_t) -> io::Result<Option<Vec<libc::proc_fdinfo>>> {
    let mut room = 64;
    loop {
        let empty = libc::proc_fdinfo {
            proc_fd: 0,
            proc_fdtype: 0,
        };
        let mut fds = vec![empty; room];
        let bytes = buffer_bytes(&fds)?;
        clear_errno();
        // SAFETY: `fds` is a writable buffer of exactly `bytes` bytes, the
        // most PROC_PIDLISTFDS writes; the call reads no other memory.
        let written = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDLISTFDS,
                0,
                fds.as_mut_ptr().cast(),
                bytes,
            )
        };
        let errno = io::Error::last_os_error();
        let count = match usize::try_from(written) {
            // 0 with errno still clear is an empty descriptor table.
            Ok(0) if errno.raw_os_error() != Some(0) => return gone_or(errno),
            Ok(written) => written / size_of::<libc::proc_fdinfo>(),
            Err(_) => return gone_or(errno),
        };
        if count < room {
            fds.truncate(count);
            return Ok(Some(fds));
        }
        room = grow(room)?;
    }
}

/// `None` for a process that is gone or not the caller's to inspect; any
/// other failure stays an error.
fn gone_or(error: io::Error) -> io::Result<Option<Vec<libc::proc_fdinfo>>> {
    match error.raw_os_error() {
        Some(libc::ESRCH | libc::EPERM) => Ok(None),
        _ => Err(error),
    }
}

/// What the kernel says about descriptor `fd` of `pid`; `None` when it was
/// closed (or its vnode revoked, or the process ended) since the listing,
/// or the kernel will not describe it to the caller.
fn vnode(pid: libc::pid_t, fd: i32) -> io::Result<Option<VnodeFdinfo>> {
    // SAFETY: `VnodeFdinfo` is plain integers and byte arrays; all-zero is
    // a valid value of it.
    let mut info: VnodeFdinfo = unsafe { std::mem::zeroed() };
    clear_errno();
    // SAFETY: `info` is a writable local of exactly `VNODE_FDINFO_SIZE`
    // bytes, what PROC_PIDFDVNODEINFO writes; the layout is pinned above.
    let written = unsafe {
        libc::proc_pidfdinfo(
            pid,
            fd,
            PROC_PIDFDVNODEINFO,
            std::ptr::addr_of_mut!(info).cast(),
            VNODE_FDINFO_SIZE,
        )
    };
    let errno = io::Error::last_os_error();
    if written == VNODE_FDINFO_SIZE {
        return Ok(Some(info));
    }
    match errno.raw_os_error() {
        // Closed, revoked or gone since the listing; or (EPERM) one the
        // kernel will not describe to this user, as it does for some of the
        // user's own system daemons (`proc_listpidspath` cannot see those
        // either).
        Some(libc::EBADF | libc::ENOENT | libc::ESRCH | libc::EPERM) => Ok(None),
        Some(0) => Err(io::Error::other("short PROC_PIDFDVNODEINFO answer")),
        _ => Err(errno),
    }
}

/// Set this thread's errno to 0.
fn clear_errno() {
    // SAFETY: `__error` answers the calling thread's errno location, valid
    // for the thread's life; storing an int there is what it is for.
    unsafe { *libc::__error() = 0 };
}

fn buffer_bytes<T>(buf: &[T]) -> io::Result<libc::c_int> {
    libc::c_int::try_from(size_of_val(buf))
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
}

fn grow(room: usize) -> io::Result<usize> {
    let next = room * 2;
    if next > LIST_MAX {
        return Err(io::Error::other(
            "the process or descriptor list keeps growing",
        ));
    }
    Ok(next)
}
