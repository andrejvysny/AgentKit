//! Directory-descriptor primitives for walking a tree below an anchored
//! root one component at a time without following links (ADR 0039: the
//! sandbox supervisor resolves every path below `/workspace` and
//! `/home/agent` this way, because the workload can write both trees).
//!
//! Complements [`crate::fs`]: [`open_dir_at_nofollow`] steps into a child
//! directory, [`create_dir_at`] makes one, [`stat_at_nofollow`] classifies
//! an entry without following it, [`read_dir_names`] lists an open
//! directory, [`create_new_rw_file_at`] creates a file that one descriptor
//! both writes and reads, and [`sync_dir`] makes a rename durable.
//!
//! Every `name` is ONE path component: a name that is empty, `.`, `..` or
//! contains `/` or NUL is refused with `InvalidInput`, so a call can never
//! resolve more than the one step its descriptor anchors.
//!
//! Unix implements the primitives; every other platform refuses with
//! [`std::io::ErrorKind::Unsupported`].

use std::ffi::OsString;
use std::fs::File;
use std::io;

/// Kind of a directory entry, read without following a link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryType {
    /// Regular file.
    File,
    /// Directory.
    Directory,
    /// Symbolic link (the link itself, never its target).
    Symlink,
    /// Anything else: FIFO, socket, device.
    Other,
}

/// What [`stat_at_nofollow`] reports about one entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntryStat {
    /// Kind of the entry itself.
    pub kind: EntryType,
    /// Apparent size in bytes (`st_size`; 0 when negative).
    pub size: u64,
    /// Space allocated on disk in bytes (`st_blocks` × 512; 0 when
    /// negative). Smaller than `size` for a sparse file, larger for one
    /// with space reserved past its end.
    pub allocated: u64,
    /// Modification time, epoch milliseconds.
    pub mtime_ms: i64,
}

/// Names of one directory, `.` and `..` left out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DirNames {
    /// Names in the order the directory returned them, at most the
    /// requested count.
    pub names: Vec<OsString>,
    /// True when the directory holds more names than were returned.
    pub truncated: bool,
}

/// Refuse anything but one plain path component.
#[cfg(unix)]
fn component(name: &str) -> io::Result<std::ffi::CString> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected one path component",
        ));
    }
    crate::fs::cstr(name)
}

/// Open the child directory `name` of the open directory `dir`, refusing a
/// link at `name` (`O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC`).
#[cfg(unix)]
pub fn open_dir_at_nofollow(dir: &File, name: &str) -> io::Result<File> {
    use std::os::fd::AsRawFd;
    let name = component(name)?;
    // SAFETY: `dir` is an open descriptor kept alive by the borrow and
    // `name` is a valid NUL-terminated single component that outlives the
    // call; the returned descriptor is wrapped into an owning `File` once.
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    crate::fs::from_fd(fd)
}

/// See [`open_dir_at_nofollow`].
#[cfg(not(unix))]
pub fn open_dir_at_nofollow(_dir: &File, _name: &str) -> io::Result<File> {
    Err(unsupported())
}

/// `mkdirat(2)`: create the directory `name` below the open directory
/// `dir` with `mode` (the umask applies). An existing entry of any kind is
/// `AlreadyExists`; nothing is followed.
#[cfg(unix)]
pub fn create_dir_at(dir: &File, name: &str, mode: u32) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let name = component(name)?;
    let mode = (mode & 0o7777) as libc::mode_t;
    // SAFETY: `dir` is an open descriptor kept alive by the borrow and
    // `name` is a valid NUL-terminated single component; `mkdirat(2)` reads
    // no other caller memory.
    crate::fs::check(unsafe { libc::mkdirat(dir.as_raw_fd(), name.as_ptr(), mode) })
}

/// See [`create_dir_at`].
#[cfg(not(unix))]
pub fn create_dir_at(_dir: &File, _name: &str, _mode: u32) -> io::Result<()> {
    Err(unsupported())
}

/// `fstatat(2)` with `AT_SYMLINK_NOFOLLOW`: what `name` below `dir` is,
/// reporting a link as [`EntryType::Symlink`] instead of its target.
#[cfg(unix)]
pub fn stat_at_nofollow(dir: &File, name: &str) -> io::Result<EntryStat> {
    use std::os::fd::AsRawFd;
    let name = component(name)?;
    // SAFETY: `stat` is a plain C struct; zeroing it is a valid value and
    // `fstatat(2)` fully writes it on success before it is read.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `dir` is an open descriptor kept alive by the borrow, `name`
    // is a valid NUL-terminated component and `st` is a writable local
    // owned by this call.
    let rc = unsafe {
        libc::fstatat(
            dir.as_raw_fd(),
            name.as_ptr(),
            &mut st,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    crate::fs::check(rc)?;
    let kind = match st.st_mode & libc::S_IFMT {
        libc::S_IFREG => EntryType::File,
        libc::S_IFDIR => EntryType::Directory,
        libc::S_IFLNK => EntryType::Symlink,
        _ => EntryType::Other,
    };
    let mtime_ms: i64 = st
        .st_mtime
        .saturating_mul(1000)
        .saturating_add(st.st_mtime_nsec / 1_000_000);
    // `st_blocks` counts 512-byte units on Linux and macOS alike.
    let allocated = u64::try_from(st.st_blocks).unwrap_or(0).saturating_mul(512);
    Ok(EntryStat {
        kind,
        size: u64::try_from(st.st_size).unwrap_or(0),
        allocated,
        mtime_ms,
    })
}

/// See [`stat_at_nofollow`].
#[cfg(not(unix))]
pub fn stat_at_nofollow(_dir: &File, _name: &str) -> io::Result<EntryStat> {
    Err(unsupported())
}

/// Exclusive create of `name` below `dir` at `mode`, opened for reading
/// AND writing through one descriptor, refusing a leaf link
/// (`O_RDWR | O_CREAT | O_EXCL | O_NOFOLLOW | O_CLOEXEC`). A log the
/// caller appends to and reads back by offset stays one inode even when
/// the name is replaced afterwards.
#[cfg(unix)]
pub fn create_new_rw_file_at(dir: &File, name: &str, mode: u32) -> io::Result<File> {
    use std::os::fd::AsRawFd;
    let name = component(name)?;
    // SAFETY: `dir` is an open descriptor kept alive by the borrow, `name`
    // is a valid NUL-terminated component, and the returned descriptor is
    // wrapped into an owning `File` exactly once.
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            mode & 0o7777,
        )
    };
    crate::fs::from_fd(fd)
}

/// See [`create_new_rw_file_at`].
#[cfg(not(unix))]
pub fn create_new_rw_file_at(_dir: &File, _name: &str, _mode: u32) -> io::Result<File> {
    Err(unsupported())
}

/// `fsync(2)` of an open directory, so a rename inside it survives a
/// crash. A filesystem that cannot sync directories (`EINVAL`) counts as
/// synced: there is nothing more the caller could do.
#[cfg(unix)]
pub fn sync_dir(dir: &File) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: `dir` is an open descriptor kept alive by the borrow;
    // `fsync(2)` reads no caller memory.
    match crate::fs::check(unsafe { libc::fsync(dir.as_raw_fd()) }) {
        Err(error) if error.raw_os_error() == Some(libc::EINVAL) => Ok(()),
        other => other,
    }
}

/// See [`sync_dir`].
#[cfg(not(unix))]
pub fn sync_dir(_dir: &File) -> io::Result<()> {
    Err(unsupported())
}

/// The names in the open directory `dir`, at most `max` of them, `.` and
/// `..` left out. Reads through a duplicate of the descriptor rewound to
/// the start, so `dir` stays usable as an anchor.
#[cfg(unix)]
pub fn read_dir_names(dir: &File, max: usize) -> io::Result<DirNames> {
    use std::os::fd::AsRawFd;
    // SAFETY: `dir` is an open descriptor kept alive by the borrow;
    // `F_DUPFD_CLOEXEC` returns a new descriptor this call owns.
    let fd = unsafe { libc::fcntl(dir.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
    if fd == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a fresh directory descriptor owned by this call; on
    // success the stream owns it and `closedir` below closes both.
    let stream = unsafe { libc::fdopendir(fd) };
    if stream.is_null() {
        let error = io::Error::last_os_error();
        // SAFETY: `fdopendir` failed, so `fd` is still ours to close.
        unsafe { libc::close(fd) };
        return Err(error);
    }
    let result = read_stream(stream, max);
    // SAFETY: `stream` came from a successful `fdopendir` and is closed
    // exactly once, after the last `readdir` on it.
    unsafe { libc::closedir(stream) };
    result
}

/// Every name of an open stream, rewound first.
#[cfg(unix)]
fn read_stream(stream: *mut libc::DIR, max: usize) -> io::Result<DirNames> {
    use std::os::unix::ffi::OsStrExt;
    // SAFETY: `stream` is a live stream owned by the caller; the
    // duplicate shares its offset with the caller's descriptor.
    unsafe { libc::rewinddir(stream) };
    let mut out = DirNames::default();
    loop {
        clear_errno();
        // SAFETY: `stream` is live; `readdir` returns a pointer valid until
        // the next call on this stream, and it is read before that call.
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            let error = io::Error::last_os_error();
            return match error.raw_os_error() {
                Some(0) | None => Ok(out),
                Some(_) if !errno_is_reliable() => Ok(out),
                Some(_) => Err(error),
            };
        }
        // SAFETY: a non-null entry points at a dirent whose `d_name` is a
        // NUL-terminated name inside the entry.
        let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) };
        let bytes = name.to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        if out.names.len() == max {
            out.truncated = true;
            return Ok(out);
        }
        out.names
            .push(std::ffi::OsStr::from_bytes(bytes).to_os_string());
    }
}

/// Zero `errno`, so a NULL from `readdir` tells end from error.
#[cfg(unix)]
fn clear_errno() {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    // SAFETY: `__errno_location` returns this thread's errno slot, which
    // is always valid to write.
    unsafe {
        *libc::__errno_location() = 0;
    }
    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
    // SAFETY: `__error` returns this thread's errno slot, which is always
    // valid to write.
    unsafe {
        *libc::__error() = 0;
    }
}

/// Whether [`clear_errno`] can zero errno on this platform; where it
/// cannot, a NULL from `readdir` reads as the end of the directory.
#[cfg(unix)]
fn errno_is_reliable() -> bool {
    cfg!(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd"
    ))
}

/// See [`read_dir_names`].
#[cfg(not(unix))]
pub fn read_dir_names(_dir: &File, _max: usize) -> io::Result<DirNames> {
    Err(unsupported())
}

#[cfg(not(unix))]
fn unsupported() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "directory-descriptor primitives require POSIX *at calls",
    )
}

#[cfg(all(test, unix))]
mod tests;
