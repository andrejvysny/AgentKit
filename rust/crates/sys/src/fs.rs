//! Descriptor-anchored file primitives: the `openat`/`renameat`/`linkat`/
//! `unlinkat`/`flock` surface the file tools and the lock-directory
//! coordinator ride instead of linking `libc` themselves.
//!
//! Confinement contract: the tools canonicalise a workspace root once, then
//! anchor every later name to an OPEN directory descriptor
//! ([`open_dir_nofollow`] + [`open_file_at_nofollow`] / [`create_new_file_at`]
//! / [`rename_at`] / [`link_at`] / [`unlink_at`]), so a path component
//! swapped for a symlink after the check cannot redirect the operation.
//! `O_NOFOLLOW` refuses a leaf symlink; `O_NONBLOCK` refuses a FIFO swap on
//! the read path. [`dir_real_path`] and [`open_parent_dir`] support the
//! "still inside the root" re-check without any path lookup.
//! [`lock_exclusive`] / [`try_lock_exclusive`] / [`unlock_exclusive`] are the
//! advisory `flock(2)` surface for lock-file guard sidecars.
//!
//! Unix implements the primitives; every other platform compiles and refuses
//! explicitly (`SysError::Unsupported` / [`std::io::ErrorKind::Unsupported`]).

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use crate::process::SysError;

/// A NUL-terminated working name; a name containing a NUL byte is refused
/// with the same `InvalidInput` the callers classified before the move.
#[cfg(unix)]
fn cstr(name: &str) -> io::Result<std::ffi::CString> {
    std::ffi::CString::new(name).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
}

/// A NUL-terminated path; the unix bytes of `path`, so non-UTF-8 names on
/// unix are as representable here as they were at the original call sites.
#[cfg(unix)]
fn cpath(path: &Path) -> io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
}

/// Wrap a fresh descriptor from a successful `open(2)`/`openat(2)` into an
/// owning `File`; `-1` maps to the last OS error.
#[cfg(unix)]
fn from_fd(fd: std::os::fd::RawFd) -> io::Result<File> {
    use std::os::fd::FromRawFd;
    if fd == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` was just returned by a successful open call and is owned
    // by no one else; `File` takes over closing it.
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// Map a `-1` return of the path syscalls to the last OS error.
#[cfg(unix)]
fn check(result: libc::c_int) -> io::Result<()> {
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Open `path` as a directory, refusing a symlink at its last component
/// (`O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC`). Pinning the result
/// as the anchor for later `*_at` calls is what closes the check-to-open
/// swap window.
#[cfg(unix)]
pub fn open_dir_nofollow(path: &Path) -> io::Result<File> {
    let path = cpath(path)?;
    // SAFETY: `path` is a valid NUL-terminated path that outlives the call;
    // `open(2)` reads no other caller memory.
    let fd = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    from_fd(fd)
}

/// See [`open_dir_nofollow`]. Platforms without POSIX open flags refuse.
#[cfg(not(unix))]
pub fn open_dir_nofollow(_path: &Path) -> io::Result<File> {
    Err(unsupported())
}

/// Open `name` below the open directory `dir`, relative to that descriptor,
/// refusing a leaf symlink and a FIFO swap
/// (`O_RDONLY | O_NOFOLLOW | O_NONBLOCK | O_CLOEXEC`).
#[cfg(unix)]
pub fn open_file_at_nofollow(dir: &File, name: &str) -> io::Result<File> {
    use std::os::fd::AsRawFd;
    let name = cstr(name)?;
    // SAFETY: `dir` is an open descriptor kept alive by the borrow and
    // `name` is a valid NUL-terminated string that outlives the call.
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    from_fd(fd)
}

/// See [`open_file_at_nofollow`].
#[cfg(not(unix))]
pub fn open_file_at_nofollow(_dir: &File, _name: &str) -> io::Result<File> {
    Err(unsupported())
}

/// Exclusive create of `name` below the open directory `dir` at `mode`,
/// refusing a leaf symlink
/// (`O_WRONLY | O_CREAT | O_EXCL | O_NOFOLLOW | O_CLOEXEC`).
#[cfg(unix)]
pub fn create_new_file_at(dir: &File, name: &str, mode: u32) -> io::Result<File> {
    use std::os::fd::AsRawFd;
    let name = cstr(name)?;
    // SAFETY: `dir` is an open descriptor kept alive by the borrow, `name`
    // is a valid NUL-terminated string, and the returned descriptor is
    // wrapped into an owning `File` exactly once.
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            mode,
        )
    };
    from_fd(fd)
}

/// See [`create_new_file_at`].
#[cfg(not(unix))]
pub fn create_new_file_at(_dir: &File, _name: &str, _mode: u32) -> io::Result<File> {
    Err(unsupported())
}

/// Open the parent of the open directory `dir` relative to the descriptor
/// itself (`openat("..", O_RDONLY | O_DIRECTORY | O_CLOEXEC)`), so the
/// identity walk needs no path lookup and works on every Unix.
#[cfg(unix)]
pub fn open_parent_dir(dir: &File) -> io::Result<File> {
    use std::os::fd::AsRawFd;
    let name = c"..";
    // SAFETY: `..` is a static NUL-terminated name and `dir` is an open
    // descriptor kept alive by the borrow.
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    from_fd(fd)
}

/// See [`open_parent_dir`].
#[cfg(not(unix))]
pub fn open_parent_dir(_dir: &File) -> io::Result<File> {
    Err(unsupported())
}

/// `renameat(2)` of two names below the open directory `dir`.
#[cfg(unix)]
pub fn rename_at(dir: &File, from: &str, to: &str) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let (from, to) = (cstr(from)?, cstr(to)?);
    let raw = dir.as_raw_fd();
    // SAFETY: `raw` is an open descriptor kept alive by the borrow and both
    // names are valid NUL-terminated strings that outlive the call.
    check(unsafe { libc::renameat(raw, from.as_ptr(), raw, to.as_ptr()) })
}

/// See [`rename_at`].
#[cfg(not(unix))]
pub fn rename_at(_dir: &File, _from: &str, _to: &str) -> io::Result<()> {
    Err(unsupported())
}

/// `linkat(2)` (flags `0`) of two names below the open directory `dir`:
/// atomic create-if-absent for the file tools' publish step.
#[cfg(unix)]
pub fn link_at(dir: &File, from: &str, to: &str) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let (from, to) = (cstr(from)?, cstr(to)?);
    let raw = dir.as_raw_fd();
    // SAFETY: `raw` is an open descriptor kept alive by the borrow and both
    // names are valid NUL-terminated strings that outlive the call.
    check(unsafe { libc::linkat(raw, from.as_ptr(), raw, to.as_ptr(), 0) })
}

/// See [`link_at`].
#[cfg(not(unix))]
pub fn link_at(_dir: &File, _from: &str, _to: &str) -> io::Result<()> {
    Err(unsupported())
}

/// `unlinkat(2)` (flags `0`) of one name below the open directory `dir`.
#[cfg(unix)]
pub fn unlink_at(dir: &File, name: &str) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let name = cstr(name)?;
    // SAFETY: `dir` is an open descriptor kept alive by the borrow and
    // `name` is a valid NUL-terminated string that outlives the call.
    check(unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), 0) })
}

/// See [`unlink_at`].
#[cfg(not(unix))]
pub fn unlink_at(_dir: &File, _name: &str) -> io::Result<()> {
    Err(unsupported())
}

/// The real filesystem location of an open DIRECTORY descriptor; `None`
/// where the platform offers no way to ask. Callers that get `None` (or a
/// path spelled differently than the root) must decide containment by
/// identity instead, e.g. walking [`open_parent_dir`].
#[cfg(unix)]
pub fn dir_real_path(dir: &File) -> io::Result<Option<PathBuf>> {
    use std::os::fd::AsRawFd;
    #[cfg(target_os = "macos")]
    {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let mut buffer = [0u8; libc::PATH_MAX as usize];
        // SAFETY: `F_GETPATH` writes a NUL-terminated path of at most
        // PATH_MAX bytes into the buffer, which is exactly PATH_MAX bytes;
        // `dir` is a live descriptor borrowed from the caller.
        let result = unsafe { libc::fcntl(dir.as_raw_fd(), libc::F_GETPATH, buffer.as_mut_ptr()) };
        if result == -1 {
            return Err(io::Error::last_os_error());
        }
        let len = buffer
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(buffer.len());
        Ok(Some(PathBuf::from(OsStr::from_bytes(&buffer[..len]))))
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        std::fs::read_link(format!("/proc/self/fd/{}", dir.as_raw_fd())).map(Some)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "android")))]
    {
        let _ = dir;
        Ok(None)
    }
}

/// See [`dir_real_path`]; the question cannot be asked without POSIX
/// descriptors, so non-unix platforms refuse rather than guess.
#[cfg(not(unix))]
pub fn dir_real_path(_dir: &File) -> io::Result<Option<PathBuf>> {
    Err(unsupported())
}

/// Take the kernel's exclusive advisory lock on an open file
/// (`flock(2)` `LOCK_EX`), blocking until the previous holder releases.
#[cfg(unix)]
pub fn lock_exclusive(file: &File) -> Result<(), SysError> {
    use std::os::fd::AsRawFd;
    // SAFETY: `file` is an open descriptor kept alive by the borrow and
    // `flock(2)` only needs the descriptor; `LOCK_EX` blocks until the
    // previous holder releases.
    check(unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) }).map_err(|source| {
        SysError::Flock {
            operation: "lock",
            source,
        }
    })
}

/// See [`lock_exclusive`]. Platforms without `flock(2)` refuse.
#[cfg(not(unix))]
pub fn lock_exclusive(_file: &File) -> Result<(), SysError> {
    Err(SysError::Unsupported)
}

/// Non-blocking [`lock_exclusive`]: `Ok(true)` when the lock was taken,
/// `Ok(false)` when someone else holds it (`EWOULDBLOCK`).
#[cfg(unix)]
pub fn try_lock_exclusive(file: &File) -> Result<bool, SysError> {
    use std::os::fd::AsRawFd;
    // SAFETY: as in `lock_exclusive`; `LOCK_NB` makes contention an
    // immediate error instead of a wait.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
        return Ok(false);
    }
    Err(SysError::Flock {
        operation: "try-lock",
        source: error,
    })
}

/// See [`try_lock_exclusive`].
#[cfg(not(unix))]
pub fn try_lock_exclusive(_file: &File) -> Result<bool, SysError> {
    Err(SysError::Unsupported)
}

/// Release a held [`lock_exclusive`] (`flock(2)` `LOCK_UN`). Closing the
/// descriptor also drops the advisory lock, so callers may treat an error
/// here as hygiene, never as continued ownership.
#[cfg(unix)]
pub fn unlock_exclusive(file: &File) -> Result<(), SysError> {
    use std::os::fd::AsRawFd;
    // SAFETY: `file` is an open descriptor kept alive by the borrow and
    // `LOCK_UN` is the documented release for a held `LOCK_EX`.
    check(unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) }).map_err(|source| {
        SysError::Flock {
            operation: "unlock",
            source,
        }
    })
}

/// See [`unlock_exclusive`].
#[cfg(not(unix))]
pub fn unlock_exclusive(_file: &File) -> Result<(), SysError> {
    Err(SysError::Unsupported)
}

#[cfg(not(unix))]
fn unsupported() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "descriptor-anchored file primitives require POSIX open flags",
    )
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    #[test]
    fn open_dir_nofollow_opens_a_directory_and_refuses_a_symlink() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("dir");
        std::fs::create_dir(&dir).unwrap();
        assert!(open_dir_nofollow(&dir).is_ok());
        std::os::unix::fs::symlink(&dir, root.path().join("link")).unwrap();
        let error = open_dir_nofollow(&root.path().join("link")).unwrap_err();
        // `O_NOFOLLOW` stops at the link node; `O_DIRECTORY` then refuses
        // it (`ENOTDIR` on apple, `ELOOP` on some other unixes). Either
        // way the link is refused, never followed.
        assert!(
            matches!(
                error.raw_os_error(),
                Some(libc::ELOOP) | Some(libc::ENOTDIR)
            ),
            "a symlinked directory must be refused: {error}"
        );
    }

    #[test]
    fn open_file_at_nofollow_reads_leaf_and_refuses_leaf_symlink() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("leaf"), b"hi").unwrap();
        std::os::unix::fs::symlink(root.path().join("leaf"), root.path().join("link")).unwrap();
        let dir = open_dir_nofollow(root.path()).unwrap();
        let mut file = open_file_at_nofollow(&dir, "leaf").unwrap();
        let mut text = String::new();
        file.read_to_string(&mut text).unwrap();
        assert_eq!(text, "hi");
        let error = open_file_at_nofollow(&dir, "link").unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::ELOOP));
        let error = open_file_at_nofollow(&dir, "missing").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn create_new_file_at_is_exclusive_and_honours_the_mode() {
        let root = tempfile::tempdir().unwrap();
        let dir = open_dir_nofollow(root.path()).unwrap();
        let file = create_new_file_at(&dir, "temp", 0o600).unwrap();
        drop(file);
        let mode = std::fs::metadata(root.path().join("temp"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let error = create_new_file_at(&dir, "temp", 0o600).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    }

    #[test]
    fn rename_link_unlink_move_names_around_the_descriptor() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("from"), b"data").unwrap();
        let dir = open_dir_nofollow(root.path()).unwrap();
        link_at(&dir, "from", "hard").unwrap();
        assert_eq!(std::fs::read(root.path().join("hard")).unwrap(), b"data");
        rename_at(&dir, "from", "to").unwrap();
        assert!(!root.path().join("from").exists());
        assert_eq!(std::fs::read(root.path().join("to")).unwrap(), b"data");
        unlink_at(&dir, "hard").unwrap();
        assert!(!root.path().join("hard").exists());
        assert_eq!(
            unlink_at(&dir, "hard").unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    #[test]
    fn open_parent_dir_walks_up_by_identity() {
        let root = tempfile::tempdir().unwrap();
        let child = root.path().join("child");
        std::fs::create_dir(&child).unwrap();
        let opened = open_dir_nofollow(&child).unwrap();
        let parent = open_parent_dir(&opened).unwrap();
        let parent_meta = parent.metadata().unwrap();
        let root_meta = std::fs::metadata(root.path()).unwrap();
        assert_eq!(parent_meta.dev(), root_meta.dev());
        assert_eq!(parent_meta.ino(), root_meta.ino());
    }

    #[test]
    fn dir_real_path_reports_the_canonical_location_or_none() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("deep/nested");
        std::fs::create_dir_all(&dir).unwrap();
        let opened = open_dir_nofollow(&dir).unwrap();
        let reported = dir_real_path(&opened).unwrap();
        #[cfg(any(target_os = "macos", target_os = "linux", target_os = "android"))]
        assert_eq!(reported, Some(std::fs::canonicalize(&dir).unwrap()));
        // Platforms without `F_GETPATH` or `/proc` report `None`, and the
        // containment check then rides identity instead.
        #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "android")))]
        assert_eq!(reported, None);
    }

    #[test]
    fn flock_exclusive_contention_then_release_then_reacquire() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("guard.flock");
        std::fs::write(&path, b"").unwrap();
        let holder = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let rival = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        lock_exclusive(&holder).unwrap();
        assert!(!try_lock_exclusive(&rival).unwrap(), "held lock contends");
        unlock_exclusive(&holder).unwrap();
        assert!(try_lock_exclusive(&rival).unwrap(), "released lock retakes");
        unlock_exclusive(&rival).unwrap();
    }
}
