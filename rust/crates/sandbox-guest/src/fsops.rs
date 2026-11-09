//! Guest filesystem operations (§8.2).
//!
//! Every path reaches disk through `openat2(RESOLVE_BENEATH |
//! RESOLVE_NO_MAGICLINKS | RESOLVE_NO_SYMLINKS)` under a per-root dirfd —
//! never `std::fs` with plain paths. Symlink/hardlink/magic-link/device
//! escapes are refused at operation time; the `GuestPath` gate is only the
//! first line.

use std::ffi::{CString, OsStr};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use agentkit_sandbox_contracts::api::messages::{GuestFsOp, GuestOpOutcome};
use agentkit_sandbox_contracts::api::{EntryKind, ListEntry, ListReply, ReadReply, WriteReceipt};
use agentkit_sandbox_contracts::ids::ArtifactDigest;
use agentkit_sandbox_contracts::limits::{
    MAX_LIST_ENTRIES, MAX_READ_REPLY_BYTES, MAX_WRITE_BODY_BYTES,
};
use agentkit_sandbox_contracts::path::{GuestPath, GuestRoot};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use sha2::{Digest, Sha256};

use crate::error::{GuestFail, code};

/// Flags passed as `open_how.resolve` for every open.
pub(crate) const RESOLVE_FLAGS: u64 =
    libc::RESOLVE_BENEATH | libc::RESOLVE_NO_MAGICLINKS | libc::RESOLVE_NO_SYMLINKS;
/// `renameat2` flag (kernel ABI constant).
pub(crate) const RENAME_NOREPLACE: libc::c_uint = 1;

/// The guest's own root directories.
#[derive(Debug, Clone)]
pub struct Roots {
    workspace: PathBuf,
    home_agent: PathBuf,
    tmp: PathBuf,
}

impl Roots {
    /// Production roots from boot params.
    #[must_use]
    pub fn new(workspace: &str) -> Self {
        Self {
            workspace: PathBuf::from(workspace),
            home_agent: PathBuf::from("/home/agent"),
            tmp: PathBuf::from("/tmp"),
        }
    }

    /// Test-mode roots overriding all three dirs (still admin-supplied,
    /// never wire-supplied; gated behind the test TCP opt-in in `main`).
    #[must_use]
    pub fn with_dirs(workspace: &str, home_agent: &str, tmp: &str) -> Self {
        Self {
            workspace: PathBuf::from(workspace),
            home_agent: PathBuf::from(home_agent),
            tmp: PathBuf::from(tmp),
        }
    }

    /// Integration-test roots pointing at real temp directories.
    #[must_use]
    pub fn for_tests(workspace: PathBuf, home_agent: PathBuf, tmp: PathBuf) -> Self {
        Self {
            workspace,
            home_agent,
            tmp,
        }
    }

    #[must_use]
    pub fn dir(&self, root: GuestRoot) -> &Path {
        match root {
            GuestRoot::Workspace => &self.workspace,
            GuestRoot::HomeAgent => &self.home_agent,
            GuestRoot::Tmp => &self.tmp,
        }
    }
}

/// Open, verified directory fds for the three roots. Construction fails
/// closed if a root is missing, not a directory, or foreign-owned.
pub struct GuestFs {
    root_fds: Vec<OwnedFd>,
    pub roots: Roots,
    pub(crate) staging: std::sync::Mutex<crate::staging::Staging>,
}

impl GuestFs {
    /// Open + verify all roots (owner root-or-self, `S_ISDIR`).
    pub fn open(roots: Roots) -> Result<Self, GuestFail> {
        let mut fds = Vec::with_capacity(3);
        for r in GuestRoot::ALL {
            fds.push(open_root_dir(roots.dir(r))?);
        }
        Ok(Self {
            root_fds: fds,
            roots,
            staging: std::sync::Mutex::new(crate::staging::Staging::default()),
        })
    }

    pub(crate) fn root_fd(&self, root: GuestRoot) -> RawFd {
        self.root_fds[root as usize].as_raw_fd()
    }

    /// Execute one wire-level fs operation.
    #[must_use]
    pub fn handle(&self, op: &GuestFsOp, _request_id: &str) -> GuestOpOutcome {
        let result = match op {
            GuestFsOp::Read { path, from, len } => self
                .read(path, *from, *len)
                .map(|reply| GuestOpOutcome::ReadOk { reply }),
            GuestFsOp::Write {
                path,
                body_b64,
                expected_sha256,
                create_parents,
            } => self
                .write(path, body_b64, expected_sha256.as_ref(), *create_parents)
                .map(|receipt| GuestOpOutcome::WriteOk { receipt }),
            GuestFsOp::List { path, options } => self
                .list(path, options.max_entries, options.include_hidden)
                .map(|reply| GuestOpOutcome::ListOk { reply }),
            GuestFsOp::Search { query } => {
                crate::search::search(self, query).map(|reply| GuestOpOutcome::SearchOk { reply })
            }
            GuestFsOp::GitInspect { .. } => Err(GuestFail::new(
                code::IO,
                "git dispatches async at the server layer",
            )),
            GuestFsOp::ImportBegin { stage_id, files } => self.import_begin(stage_id, files),
            GuestFsOp::ImportData {
                stage_id,
                relative_path,
                sequence,
                bytes_b64,
                chunk_digest,
            } => self.import_data(stage_id, relative_path, *sequence, bytes_b64, chunk_digest),
            GuestFsOp::ImportEnd { stage_id } => self.import_end(stage_id),
            GuestFsOp::ExportGather { stage_id, paths } => self.export_gather(stage_id, paths),
            GuestFsOp::ExportFetch {
                stage_id,
                relative_path,
                sequence,
            } => self.export_fetch(stage_id, relative_path, *sequence),
        };
        match result {
            Ok(ok) => ok,
            Err(f) => GuestOpOutcome::Failed {
                code: f.code.to_string(),
                message: f.message,
            },
        }
    }

    fn open_target(&self, path: &GuestPath, flags: libc::c_int) -> Result<OwnedFd, GuestFail> {
        let rel = relative_cstr(path)?;
        openat2(self.root_fd(path.root()), &rel, flags, 0).map_err(|e| classify_open(&e))
    }

    // -- Read ---------------------------------------------------------------

    fn read(
        &self,
        path: &GuestPath,
        from: Option<u64>,
        len: Option<u64>,
    ) -> Result<ReadReply, GuestFail> {
        let fd = self.open_target(
            path,
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
        )?;
        let st = fstat(fd.as_raw_fd())?;
        if (st.st_mode & libc::S_IFMT) != libc::S_IFREG {
            return Err(GuestFail::path_escape("read target is not a regular file"));
        }
        let total = st.st_size.max(0) as u64;
        let from = from.unwrap_or(0);
        if from > total {
            return Err(GuestFail::new(code::BAD_INPUT, "from beyond eof"));
        }
        let remaining = total - from;
        let want = len.unwrap_or(remaining).min(remaining);
        let take =
            usize::try_from(want.min(MAX_READ_REPLY_BYTES as u64)).unwrap_or(MAX_READ_REPLY_BYTES);
        let mut buf = vec![0u8; take];
        let mut got = 0usize;
        while got < take {
            // SAFETY: pread into allocated buffer of `take`.
            let n = unsafe {
                libc::pread(
                    fd.as_raw_fd(),
                    buf[got..].as_mut_ptr().cast(),
                    take - got,
                    (from as i64).checked_add(got as i64).unwrap_or(i64::MAX),
                )
            };
            if n < 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(GuestFail::io("pread", &e));
            }
            if n == 0 {
                break;
            }
            got += usize::try_from(n).unwrap_or(0);
        }
        buf.truncate(got);
        let end = from + got as u64;
        let modified_ms = u64::try_from(st.st_mtime)
            .ok()
            .map(|s| s * 1000 + st_mtime_nsec(&st));
        Ok(ReadReply {
            data_b64: STANDARD.encode(&buf),
            sha256: ArtifactDigest::of_bytes(&buf),
            total_size: total,
            from,
            next_from: if end < total { Some(end) } else { None },
            truncated: end < total,
            modified_ms,
        })
    }

    // -- Write --------------------------------------------------------------

    fn write(
        &self,
        path: &GuestPath,
        body_b64: &str,
        expected_sha256: Option<&ArtifactDigest>,
        create_parents: bool,
    ) -> Result<WriteReceipt, GuestFail> {
        if body_b64.len() > MAX_WRITE_BODY_BYTES / 3 * 4 + 8 {
            return Err(GuestFail::new(code::BODY_TOO_LARGE, "body too large"));
        }
        let body = STANDARD
            .decode(body_b64)
            .map_err(|_| GuestFail::new(code::BAD_BASE64, "strict base64 decode failed"))?;
        if body.len() > MAX_WRITE_BODY_BYTES {
            return Err(GuestFail::new(code::BODY_TOO_LARGE, "body too large"));
        }

        let mut comps: Vec<String> = path.components().map(str::to_string).collect();
        if comps.is_empty() {
            return Err(GuestFail::path_escape("write target is the root itself"));
        }
        let file_name = comps.pop().unwrap_or_default();
        let root = path.root();
        let parent_fd = self.walk_dir(root, &comps, create_parents)?;
        let name = cstr(&file_name)?;

        // Inspect existing target (O_PATH|NOFOLLOW: never follows symlinks).
        let existing = match openat2(
            parent_fd.as_raw_fd(),
            &name,
            libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0,
        ) {
            Ok(fd) => {
                let st = fstat(fd.as_raw_fd())?;
                if (st.st_mode & libc::S_IFMT) != libc::S_IFREG {
                    return Err(GuestFail::path_escape(
                        "write target exists but is not a regular file",
                    ));
                }
                Some(st)
            }
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => None,
            Err(e) => return Err(classify_open(&e)),
        };

        // CAS: hash existing content before replacing.
        if let Some(expected) = expected_sha256 {
            if existing.is_none() {
                return Err(GuestFail::new(
                    code::DIGEST_MISMATCH,
                    "expected digest but target absent",
                ));
            }
            let cur = read_digest_beneath(parent_fd.as_raw_fd(), &name)?;
            if &cur != expected {
                return Err(GuestFail::new(code::DIGEST_MISMATCH, "cas digest mismatch"));
            }
        }

        let created = existing.is_none();
        let final_mode = existing.map_or(0o644, |st| st.st_mode & 0o777);

        let tmp_name = cstr(&format!(".agentkit-tmp-{}", random_hex()))?;
        let tmp_fd = openat(
            parent_fd.as_raw_fd(),
            &tmp_name,
            libc::O_CREAT | libc::O_EXCL | libc::O_WRONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            0o600,
        )
        .map_err(|e| GuestFail::io("open temp", &e))?;
        write_all(tmp_fd.as_raw_fd(), &body)?;
        // SAFETY: valid fd.
        unsafe { libc::fsync(tmp_fd.as_raw_fd()) };
        // SAFETY: valid fd + mode.
        if unsafe { libc::fchmod(tmp_fd.as_raw_fd(), libc::mode_t::from(final_mode)) } != 0 {
            let e = std::io::Error::last_os_error();
            return Err(GuestFail::io("fchmod", &e));
        }
        let flags = if created { RENAME_NOREPLACE } else { 0 };
        // SAFETY: valid fds/names; renameat2 gives the atomic swap.
        let rc = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                parent_fd.as_raw_fd(),
                tmp_name.as_ptr(),
                parent_fd.as_raw_fd(),
                name.as_ptr(),
                flags,
            )
        };
        if rc != 0 {
            let e = std::io::Error::last_os_error();
            // SAFETY: unlink the orphaned temp name.
            unsafe { libc::unlinkat(parent_fd.as_raw_fd(), tmp_name.as_ptr(), 0) };
            if e.raw_os_error() == Some(libc::EEXIST) && created {
                return Err(GuestFail::new(
                    code::DIGEST_MISMATCH,
                    "concurrent creation raced the CAS window",
                ));
            }
            return Err(GuestFail::io("renameat2", &e));
        }
        // SAFETY: best-effort durability of the directory entry.
        unsafe { libc::fsync(parent_fd.as_raw_fd()) };

        Ok(WriteReceipt {
            path: path.clone(),
            sha256: ArtifactDigest::of_bytes(&body),
            bytes_written: body.len() as u64,
            created,
        })
    }

    // -- List ---------------------------------------------------------------

    fn list(
        &self,
        path: &GuestPath,
        max_entries: u32,
        include_hidden: bool,
    ) -> Result<ListReply, GuestFail> {
        let comps: Vec<String> = path.components().map(str::to_string).collect();
        let dir_fd = self.open_dir(path.root(), &comps)?;
        let cap = usize::try_from(max_entries)
            .unwrap_or(MAX_LIST_ENTRIES)
            .min(MAX_LIST_ENTRIES);
        list_dir(dir_fd, cap, include_hidden)
    }

    // -- shared traversal ---------------------------------------------------

    /// Walk (optionally creating) the directory chain beneath a root fd.
    pub(crate) fn walk_dir(
        &self,
        root: GuestRoot,
        comps: &[String],
        create: bool,
    ) -> Result<OwnedFd, GuestFail> {
        let mut fd = reopen_dir(self.roots.dir(root))?;
        for comp in comps {
            let name = cstr(comp)?;
            loop {
                match openat2(
                    fd.as_raw_fd(),
                    &name,
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    0,
                ) {
                    Ok(next) => {
                        fd = next;
                        break;
                    }
                    Err(e) if e.raw_os_error() == Some(libc::ENOENT) && create => {
                        // SAFETY: mkdirat with mode on a valid dirfd.
                        let rc = unsafe { libc::mkdirat(fd.as_raw_fd(), name.as_ptr(), 0o700) };
                        if rc != 0
                            && std::io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST)
                        {
                            let e2 = std::io::Error::last_os_error();
                            return Err(GuestFail::io("mkdirat", &e2));
                        }
                    }
                    Err(e) => return Err(classify_open(&e)),
                }
            }
        }
        let st = fstat(fd.as_raw_fd())?;
        if (st.st_mode & libc::S_IFMT) != libc::S_IFDIR {
            return Err(GuestFail::path_escape("parent chain is not a directory"));
        }
        Ok(fd)
    }

    /// Open a directory target, verifying `S_ISDIR`.
    pub(crate) fn open_dir(&self, root: GuestRoot, comps: &[String]) -> Result<OwnedFd, GuestFail> {
        self.walk_dir(root, comps, false)
    }

    /// Canonical on-disk path for a guest path, derived from the *verified*
    /// root mapping (never from wire strings). Verified via `open_dir` first.
    pub fn real_path(&self, path: &GuestPath) -> Result<std::path::PathBuf, GuestFail> {
        let root = path.root();
        let comps: Vec<String> = path.components().map(str::to_string).collect();
        let _fd = self.open_dir(root, &comps)?;
        let mut p = self.roots.dir(root).to_path_buf();
        for c in &comps {
            p.push(c);
        }
        Ok(p)
    }
}

// ---------------------------------------------------------------------------
// syscall helpers
// ---------------------------------------------------------------------------

pub(crate) fn cstr(s: &str) -> Result<CString, GuestFail> {
    if s.is_empty() {
        return Err(GuestFail::path_escape("empty name"));
    }
    if s == "." || s == ".." {
        return Err(GuestFail::path_escape("dot segment"));
    }
    CString::new(s.as_bytes()).map_err(|_| GuestFail::path_escape("NUL in name"))
}

fn relative_cstr(path: &GuestPath) -> Result<CString, GuestFail> {
    let rel: String = path.components().collect::<Vec<_>>().join("/");
    if rel.is_empty() {
        return Err(GuestFail::path_escape("empty relative path"));
    }
    CString::new(rel.as_bytes()).map_err(|_| GuestFail::path_escape("NUL in path"))
}

fn path_to_cstring(p: &Path) -> Result<CString, GuestFail> {
    CString::new(OsStr::as_bytes(p.as_os_str())).map_err(|_| GuestFail::path_escape("NUL in path"))
}

/// `openat2` with the mandated resolve flags.
pub(crate) fn openat2(
    dirfd: RawFd,
    path: &CString,
    flags: libc::c_int,
    mode: u32,
) -> std::io::Result<OwnedFd> {
    let mut how: libc::open_how = unsafe {
        // SAFETY: all-zero open_how is a valid initialization; fields set next.
        std::mem::zeroed()
    };
    how.flags = u64::try_from(flags).unwrap_or(0);
    how.mode = u64::from(mode);
    how.resolve = RESOLVE_FLAGS;
    // SAFETY: openat2 syscall; kernel validates dirfd and open_how size.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            dirfd,
            path.as_ptr(),
            std::ptr::from_ref(&how),
            std::mem::size_of::<libc::open_how>(),
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: fresh descriptor, owned exactly once.
    Ok(unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
}

pub(crate) fn openat(
    dirfd: RawFd,
    path: &CString,
    flags: libc::c_int,
    mode: u32,
) -> std::io::Result<OwnedFd> {
    // SAFETY: standard openat with valid dirfd and NUL-terminated name.
    let fd = unsafe { libc::openat(dirfd, path.as_ptr(), flags, mode) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: fresh descriptor, owned exactly once.
    Ok(unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
}

pub(crate) fn fstat(fd: RawFd) -> Result<libc::stat, GuestFail> {
    let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: fstat fills the struct for a valid fd.
    let rc = unsafe { libc::fstat(fd, st.as_mut_ptr()) };
    if rc != 0 {
        return Err(GuestFail::io("fstat", &std::io::Error::last_os_error()));
    }
    // SAFETY: fstat succeeded; struct initialized.
    Ok(unsafe { st.assume_init() })
}

pub(crate) fn st_mtime_nsec(_st: &libc::stat) -> u64 {
    // Second-granularity from st_mtime is sufficient for modified_ms; nsec is
    // intentionally zero to stay libc-struct-agnostic.
    0
}

/// Map open errors: capability/loop/overflow failures are escape attempts.
#[must_use]
pub fn classify_open(e: &std::io::Error) -> GuestFail {
    match e.raw_os_error() {
        // RESOLVE_BENEATH escapes surface as EXDEV; symlink loops as ELOOP;
        // ENOTCAPABLE (=EOVERFLOW value on some libc) is fail-closed too.
        Some(libc::ELOOP) | Some(libc::EOVERFLOW) | Some(libc::EXDEV) => {
            GuestFail::path_escape("openat2 refused")
        }
        Some(libc::EBADF) => GuestFail::path_escape("root fd closed"),
        Some(libc::ENOENT) => GuestFail::new(code::NOT_FOUND, "no such path"),
        Some(libc::ENOTDIR) => GuestFail::path_escape("path component is not a directory"),
        _ => GuestFail::io("openat2", e),
    }
}

fn open_root_dir(p: &Path) -> Result<OwnedFd, GuestFail> {
    let c = path_to_cstring(p)?;
    let fd = openat(
        libc::AT_FDCWD,
        &c,
        libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
        0,
    )
    .map_err(|e| GuestFail::io("root open", &e))?;
    let st = fstat(fd.as_raw_fd())?;
    if (st.st_mode & libc::S_IFMT) != libc::S_IFDIR {
        return Err(GuestFail::path_escape("root is not a directory"));
    }
    // SAFETY: geteuid always valid.
    let uid = unsafe { libc::geteuid() };
    if st.st_uid != 0 && st.st_uid != uid {
        return Err(GuestFail::path_escape("root not owned by root/self"));
    }
    Ok(fd)
}

/// Reopen a root as a traversable `O_RDONLY` dirfd (O_PATH fds cannot readdir).
pub(crate) fn reopen_dir(p: &Path) -> Result<OwnedFd, GuestFail> {
    let c = path_to_cstring(p)?;
    openat(
        libc::AT_FDCWD,
        &c,
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        0,
    )
    .map_err(|e| GuestFail::io("root reopen", &e))
}

pub(crate) fn write_all(fd: RawFd, mut buf: &[u8]) -> Result<(), GuestFail> {
    while !buf.is_empty() {
        // SAFETY: write from a valid slice.
        let n = unsafe { libc::write(fd, buf.as_ptr().cast(), buf.len()) };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(GuestFail::io("write", &e));
        }
        let n = usize::try_from(n).unwrap_or(0);
        buf = &buf[n..];
    }
    Ok(())
}

/// Digest of a name beneath a dirfd via `openat2` (CAS path).
fn read_digest_beneath(parent_fd: RawFd, name: &CString) -> Result<ArtifactDigest, GuestFail> {
    const CAS_READ_CAP: i64 = 64 * 1024 * 1024;
    let fd = openat2(
        parent_fd,
        name,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
        0,
    )
    .map_err(|e| classify_open(&e))?;
    let st = fstat(fd.as_raw_fd())?;
    if (st.st_mode & libc::S_IFMT) != libc::S_IFREG {
        return Err(GuestFail::path_escape("cas target is not a regular file"));
    }
    if st.st_size > CAS_READ_CAP {
        return Err(GuestFail::new(
            code::DIGEST_MISMATCH,
            "existing file too large to verify",
        ));
    }
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut off: i64 = 0;
    loop {
        // SAFETY: pread into initialized buffer at tracked offset.
        let n = unsafe { libc::pread(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), off) };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(GuestFail::io("pread", &e));
        }
        if n == 0 {
            break;
        }
        let n = usize::try_from(n).unwrap_or(0);
        hasher.update(&buf[..n]);
        off += n as i64;
    }
    let digest_hex: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    ArtifactDigest::parse(&format!("sha256:{digest_hex}"))
        .map_err(|_| GuestFail::new(code::IO, "digest formatting"))
}

/// 8 random hex chars from the kernel CSPRNG for temp names.
pub(crate) fn random_hex() -> String {
    let mut b = [0u8; 8];
    // SAFETY: getrandom into a valid buffer.
    let ok = unsafe { libc::getrandom(b.as_mut_ptr().cast(), b.len(), 0) } == b.len() as isize;
    if !ok {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64);
        b = nanos.to_le_bytes();
    }
    let mut s = String::with_capacity(16);
    for x in b {
        s.push_str(&format!("{x:02x}"));
    }
    s
}

/// `fdopendir`-based listing over a verified dirfd.
pub fn list_dir(
    dir_fd: OwnedFd,
    max_entries: usize,
    include_hidden: bool,
) -> Result<ListReply, GuestFail> {
    let raw = dir_fd.as_raw_fd();
    // SAFETY: dup + fdopendir transfer the duped fd's ownership.
    let dupfd = unsafe { libc::dup(raw) };
    if dupfd < 0 {
        return Err(GuestFail::io("dup", &std::io::Error::last_os_error()));
    }
    let dp = unsafe { libc::fdopendir(dupfd) };
    if dp.is_null() {
        // SAFETY: fdopendir failed, so the duped fd is still ours to close.
        unsafe { libc::close(dupfd) };
        return Err(GuestFail::io("fdopendir", &std::io::Error::last_os_error()));
    }
    let mut entries: Vec<ListEntry> = Vec::new();
    let mut truncated = false;
    loop {
        // SAFETY: readdir on a valid DIR from fdopendir.
        let ent = unsafe { libc::readdir(dp) };
        if ent.is_null() {
            break;
        }
        // SAFETY: dirent valid until next readdir call.
        let d = unsafe { &*ent };
        // SAFETY: d_name is a NUL-terminated C string.
        let name_c = unsafe { std::ffi::CStr::from_ptr(d.d_name.as_ptr()) };
        let Ok(name) = name_c.to_str() else { continue };
        if name == "." || name == ".." || (!include_hidden && name.starts_with('.')) {
            continue;
        }
        if entries.len() >= max_entries {
            truncated = true;
            break;
        }
        let Ok(name_nul) = CString::new(name) else {
            continue;
        };
        let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: fstatat into uninit struct; valid dirfd + name.
        let rc = unsafe {
            libc::fstatat(
                raw,
                name_nul.as_ptr(),
                st.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        let (kind, size) = if rc == 0 {
            // SAFETY: fstatat succeeded.
            let st = unsafe { st.assume_init() };
            (stat_kind(&st), st.st_size.max(0) as u64)
        } else {
            (other_kind_from_dtype(d.d_type), 0)
        };
        entries.push(ListEntry {
            name: name.to_string(),
            kind,
            size,
        });
    }
    // SAFETY: closedir for the fdopendir handle.
    unsafe { libc::closedir(dp) };
    Ok(ListReply { entries, truncated })
}

#[must_use]
pub fn stat_kind(st: &libc::stat) -> EntryKind {
    match st.st_mode & libc::S_IFMT {
        libc::S_IFREG => EntryKind::File,
        libc::S_IFDIR => EntryKind::Dir,
        libc::S_IFLNK => EntryKind::Symlink,
        _ => EntryKind::Other,
    }
}

fn other_kind_from_dtype(d_type: u8) -> EntryKind {
    #[cfg(target_os = "linux")]
    match d_type {
        libc::DT_DIR => EntryKind::Dir,
        libc::DT_REG => EntryKind::File,
        libc::DT_LNK => EntryKind::Symlink,
        _ => EntryKind::Other,
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = d_type;
        EntryKind::Other
    }
}
