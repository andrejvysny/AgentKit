//! Streaming artifact staging (§11.1 imports, §11.2 exports).
//!
//! Imports: `ImportBegin` pins an immutable expected-file list; chunked
//! `ImportData` records accumulate in memory per file (bounded by the
//! contract caps, re-checked here — the guest never trusts the worker);
//! `ImportEnd` verifies every final size + SHA-256 and only then commits
//! each file with `openat2` beneath the workspace rootfd, `O_EXCL` create
//! and `renameat2(NO_REPLACE)`. Nothing imported is visible before its
//! digest verifies; a failed end leaves the workspace untouched.
//!
//! Exports: `ExportGather` opens every requested path beneath its root with
//! `openat2(RESOLVE_BENEATH|NO_MAGICLINKS|NO_SYMLINKS)`, refuses anything
//! that is not a regular file, hashes it, and freezes a sorted,
//! digest-pinned manifest. `ExportFetch` re-opens the path, compares
//! `(dev, ino, size, mtime)` against the pin, and serves one bounded chunk
//! per call — a file mutated after gather fails closed.

use std::collections::HashMap;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};

use agentkit_sandbox_contracts::api::messages::GuestOpOutcome;
use agentkit_sandbox_contracts::api::{
    StageDirection, StageFile, StageManifest, validate_stage_relative_path,
};
use agentkit_sandbox_contracts::ids::{ArtifactDigest, StageId};
use agentkit_sandbox_contracts::limits::{MAX_STAGE_CHUNK_BYTES, MAX_STAGED_TOTAL_BYTES};
use sha2::{Digest, Sha256};

use crate::error::{GuestFail, code};
use crate::fsops::{
    GuestFs, RENAME_NOREPLACE, classify_open, cstr, fstat, openat2, random_hex, write_all,
};

/// Upper bound on files per staged manifest (mirrors worker-side policy).
pub const MAX_STAGED_FILES: usize = 10_000;

#[derive(Default)]
pub struct Staging {
    import: Option<ImportSession>,
    export: Option<ExportSession>,
}

struct ImportSession {
    stage_id: StageId,
    expected: Vec<StageFile>,
    /// relative_path -> (accumulated bytes, next expected sequence)
    bufs: HashMap<String, (Vec<u8>, u32)>,
}

struct ExportSession {
    stage_id: StageId,
    manifest: StageManifest,
    /// relative_path -> identity pin taken at gather time.
    pinned: HashMap<String, Pin>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Pin {
    dev: u64,
    ino: u64,
    size: u64,
    mtime: i64,
}

fn pins_match(st: &libc::stat, pin: &Pin) -> bool {
    st.st_dev == pin.dev
        && st.st_ino == pin.ino
        && st.st_size as u64 == pin.size
        && st.st_mtime == pin.mtime
}

/// SHA-256 of an opened regular file via pread (bounded by staged caps).
fn hash_fd(fd: RawFd, size: u64) -> Result<ArtifactDigest, GuestFail> {
    if size > MAX_STAGED_TOTAL_BYTES {
        return Err(GuestFail::new(
            code::BODY_TOO_LARGE,
            "file exceeds staged cap",
        ));
    }
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut off: i64 = 0;
    loop {
        // SAFETY: pread into a valid buffer at a tracked offset.
        let n = unsafe { libc::pread(fd, buf.as_mut_ptr().cast(), buf.len(), off) };
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
    let hex: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    ArtifactDigest::parse(&format!("sha256:{hex}"))
        .map_err(|_| GuestFail::new(code::IO, "digest formatting"))
}

fn unb64(s: &str) -> Result<Vec<u8>, GuestFail> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .map_err(|_| GuestFail::new(code::BAD_BASE64, "chunk not base64"))
}

fn b64(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Resolve (creating as needed) the directory for `rel` beneath `root_fd`,
/// returning the final parent dirfd. Every component goes through
/// `openat2` with the resolve flags; a non-directory component is refused.
fn ensure_parent_beneath(
    root_fd: RawFd,
    rel: &str,
) -> Result<(std::fs::File, Vec<std::fs::File>), GuestFail> {
    let segs: Vec<&str> = rel.split('/').collect();
    // Intermediate dirfds must stay open for the whole walk (and for the
    // caller's use of the returned parent), so keep them all alive here.
    let mut keep: Vec<std::fs::File> = Vec::new();
    let mut cur: RawFd = root_fd;
    for seg in &segs[..segs.len().saturating_sub(1)] {
        let name = cstr(seg)?;
        let opened = match openat2(
            cur,
            &name,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0,
        ) {
            Ok(fd) => fd,
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => {
                // openat2 refuses O_CREAT|O_DIRECTORY (EINVAL), so create via
                // mkdirat at the trusted dirfd (a planted symlink at the
                // final component makes mkdirat fail EEXIST) and re-open
                // through openat2 + S_IFDIR check — never trust the name.
                // SAFETY: mkdirat at a valid dirfd with NUL name.
                if unsafe { libc::mkdirat(cur, name.as_ptr(), 0o700) } != 0 {
                    let e2 = std::io::Error::last_os_error();
                    if e2.raw_os_error() != Some(libc::EEXIST) {
                        return Err(classify_open(&e2));
                    }
                }
                match openat2(
                    cur,
                    &name,
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    0,
                ) {
                    Ok(fd) => fd,
                    Err(e2) => return Err(classify_open(&e2)),
                }
            }
            Err(e) => return Err(classify_open(&e)),
        };
        let st = fstat(opened.as_raw_fd())?;
        if (st.st_mode & libc::S_IFMT) != libc::S_IFDIR {
            return Err(GuestFail::path_escape("path component is not a directory"));
        }
        let file = std::fs::File::from(opened);
        cur = file.as_raw_fd();
        keep.push(file);
    }
    let last = match keep.pop() {
        Some(f) => f,
        // Single-component rel: the workspace root itself is the parent.
        None => {
            // SAFETY: dup of a valid fd.
            let fd = unsafe { libc::dup(root_fd) };
            if fd < 0 {
                return Err(GuestFail::io("dup", &std::io::Error::last_os_error()));
            }
            // SAFETY: fresh owned descriptor.
            unsafe { std::fs::File::from_raw_fd(fd) }
        }
    };
    Ok((last, keep))
}

/// Open an existing regular file beneath `root_fd` at `rel`, refusing
/// symlinks/dirs/devices at every component.
fn open_regular_beneath(
    root_fd: RawFd,
    rel: &str,
) -> Result<(std::fs::File, libc::stat), GuestFail> {
    let segs: Vec<&str> = rel.split('/').collect();
    let mut keep: Vec<std::fs::File> = Vec::new();
    let mut cur: RawFd = root_fd;
    for seg in &segs[..segs.len().saturating_sub(1)] {
        let name = cstr(seg)?;
        let fd = openat2(
            cur,
            &name,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0,
        )
        .map_err(|e| classify_open(&e))?;
        cur = fd.as_raw_fd();
        keep.push(std::fs::File::from(fd));
    }
    let name = cstr(segs[segs.len() - 1])?;
    let fd = openat2(
        cur,
        &name,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
        0,
    )
    .map_err(|e| classify_open(&e))?;
    let st = fstat(fd.as_raw_fd())?;
    if (st.st_mode & libc::S_IFMT) != libc::S_IFREG {
        return Err(GuestFail::new(code::NOT_A_FILE, "not a regular file"));
    }
    let _ = keep;
    Ok((std::fs::File::from(fd), st))
}

impl GuestFs {
    pub(crate) fn import_begin(
        &self,
        stage_id: &StageId,
        files: &[StageFile],
    ) -> Result<GuestOpOutcome, GuestFail> {
        let mut st = self
            .staging
            .lock()
            .map_err(|_| GuestFail::new(code::IO, "staging state poisoned; restart guest"))?;
        // A new begin replaces any unfinished session (partial buffers are
        // dropped): a stalled import must never wedge the staging slot.
        if st.import.is_some() {
            st.import = None;
        }
        if files.len() > MAX_STAGED_FILES {
            return Err(GuestFail::new(
                code::BODY_TOO_LARGE,
                "too many staged files",
            ));
        }
        let mut total: u64 = 0;
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for f in files {
            validate_stage_relative_path(&f.relative_path)
                .map_err(|e| GuestFail::new(code::PATH_ESCAPE, e))?;
            if !seen.insert(f.relative_path.as_str()) {
                return Err(GuestFail::new(code::BAD_INPUT, "duplicate staged path"));
            }
            if f.size > MAX_STAGED_TOTAL_BYTES {
                return Err(GuestFail::new(
                    code::BODY_TOO_LARGE,
                    "file exceeds staged cap",
                ));
            }
            total = total
                .checked_add(f.size)
                .ok_or_else(|| GuestFail::new(code::BODY_TOO_LARGE, "staged total overflow"))?;
        }
        if total > MAX_STAGED_TOTAL_BYTES {
            return Err(GuestFail::new(
                code::BODY_TOO_LARGE,
                "staged total exceeds cap",
            ));
        }
        st.import = Some(ImportSession {
            stage_id: stage_id.clone(),
            expected: files.to_vec(),
            bufs: HashMap::new(),
        });
        Ok(GuestOpOutcome::Ack {})
    }

    pub(crate) fn import_data(
        &self,
        stage_id: &StageId,
        relative_path: &str,
        sequence: u32,
        bytes_b64: &str,
        chunk_digest: &ArtifactDigest,
    ) -> Result<GuestOpOutcome, GuestFail> {
        let bytes = unb64(bytes_b64)?;
        if bytes.len() > MAX_STAGE_CHUNK_BYTES {
            return Err(GuestFail::new(code::BODY_TOO_LARGE, "chunk exceeds cap"));
        }
        if ArtifactDigest::of_bytes(&bytes) != *chunk_digest {
            return Err(GuestFail::new(
                code::DIGEST_MISMATCH,
                "chunk digest mismatch",
            ));
        }
        let mut st = self
            .staging
            .lock()
            .map_err(|_| GuestFail::new(code::IO, "staging state poisoned; restart guest"))?;
        let Some(session) = st.import.as_mut() else {
            return Err(GuestFail::new(
                code::NOT_STARTED,
                "no active import (begin first)",
            ));
        };
        if &session.stage_id != stage_id {
            return Err(GuestFail::new(code::BAD_INPUT, "stage id mismatch"));
        }
        let spec = session
            .expected
            .iter()
            .find(|f| f.relative_path == relative_path)
            .ok_or_else(|| GuestFail::new(code::NOT_FOUND, "file not in import manifest"))?;
        let entry = session
            .bufs
            .entry(relative_path.to_string())
            .or_insert_with(|| (Vec::new(), 0));
        if entry.1 != sequence {
            return Err(GuestFail::new(code::BAD_INPUT, "chunk sequence gap"));
        }
        if entry.0.len() as u64 + bytes.len() as u64 > spec.size {
            return Err(GuestFail::new(
                code::BODY_TOO_LARGE,
                "file overran manifest size",
            ));
        }
        entry.0.extend_from_slice(&bytes);
        entry.1 += 1;
        Ok(GuestOpOutcome::Ack {})
    }

    pub(crate) fn import_end(&self, stage_id: &StageId) -> Result<GuestOpOutcome, GuestFail> {
        // Take the session first: verification must not hold the lock while
        // touching the filesystem for long, and a failed commit must drop it.
        let mut st = self
            .staging
            .lock()
            .map_err(|_| GuestFail::new(code::IO, "staging state poisoned; restart guest"))?;
        let Some(session) = st.import.as_ref() else {
            return Err(GuestFail::new(code::NOT_STARTED, "no active import"));
        };
        if &session.stage_id != stage_id {
            return Err(GuestFail::new(code::BAD_INPUT, "stage id mismatch"));
        }
        // Verify everything before committing anything (all-or-nothing).
        for f in &session.expected {
            let (buf, _seq) = session.bufs.get(&f.relative_path).ok_or_else(|| {
                GuestFail::new(
                    code::BAD_INPUT,
                    format!("missing chunks for {}", f.relative_path),
                )
            })?;
            if buf.len() as u64 != f.size {
                return Err(GuestFail::new(
                    code::DIGEST_MISMATCH,
                    format!("size mismatch for {}", f.relative_path),
                ));
            }
            if ArtifactDigest::of_bytes(buf) != f.sha256 {
                return Err(GuestFail::new(
                    code::DIGEST_MISMATCH,
                    format!("final digest mismatch for {}", f.relative_path),
                ));
            }
        }
        let root_fd = self.root_fd(agentkit_sandbox_contracts::path::GuestRoot::Workspace);
        let mut copied = 0u32;
        for f in &session.expected {
            let (buf, _) = session.bufs.get(&f.relative_path).expect("verified above");
            let (parent, _keep) = ensure_parent_beneath(root_fd, &f.relative_path)?;
            let segs: Vec<&str> = f.relative_path.split('/').collect();
            let fname = segs[segs.len() - 1];
            let tmp_name = cstr(&format!(".akimp-{}-{fname}", random_hex()))?;
            let final_name = cstr(fname)?;
            let fd = openat2(
                parent.as_raw_fd(),
                &tmp_name,
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
            .map_err(|e| classify_open(&e))?;
            write_all(fd.as_raw_fd(), buf)?;
            // SAFETY: fchmod on an owned fd.
            if unsafe { libc::fchmod(fd.as_raw_fd(), 0o600) } != 0 {
                let e = std::io::Error::last_os_error();
                return Err(GuestFail::io("fchmod", &e));
            }
            // SAFETY: fsync on an owned fd; durability before visibility.
            if unsafe { libc::fsync(fd.as_raw_fd()) } != 0 {
                let e = std::io::Error::last_os_error();
                return Err(GuestFail::io("fsync", &e));
            }
            drop(fd);
            // SAFETY: renameat2 with valid dirfds and NUL names; NOREPLACE
            // makes an existing target fail rather than clobber.
            let rc = unsafe {
                libc::syscall(
                    libc::SYS_renameat2,
                    parent.as_raw_fd(),
                    tmp_name.as_ptr(),
                    parent.as_raw_fd(),
                    final_name.as_ptr(),
                    RENAME_NOREPLACE,
                )
            };
            if rc != 0 {
                let e = std::io::Error::last_os_error();
                // Clean the temp file on failure (best effort).
                // SAFETY: unlinkat at the verified parent.
                unsafe {
                    libc::unlinkat(parent.as_raw_fd(), tmp_name.as_ptr(), 0);
                }
                if e.raw_os_error() == Some(libc::EEXIST) {
                    return Err(GuestFail::new(
                        code::BAD_INPUT,
                        format!("refusing to overwrite existing {}", f.relative_path),
                    ));
                }
                return Err(GuestFail::io("renameat2", &e));
            }
            copied += 1;
        }
        st.import = None;
        Ok(GuestOpOutcome::ImportOk { copied })
    }

    pub(crate) fn export_gather(
        &self,
        stage_id: &StageId,
        paths: &[agentkit_sandbox_contracts::path::GuestPath],
    ) -> Result<GuestOpOutcome, GuestFail> {
        let mut files_out: Vec<StageFile> = Vec::new();
        let mut pinned: HashMap<String, Pin> = HashMap::new();
        let mut total: u64 = 0;
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for p in paths {
            let root = p.root();
            if !matches!(root, agentkit_sandbox_contracts::path::GuestRoot::Workspace) {
                // Exports are workspace-scoped (§11.2); home/tmp staging is
                // not part of the artifact surface.
                return Err(GuestFail::new(
                    code::PATH_ESCAPE,
                    "export paths must live under /workspace",
                ));
            }
            let rel = p
                .as_str()
                .strip_prefix("/workspace/")
                .ok_or_else(|| GuestFail::new(code::PATH_ESCAPE, "export path shape"))?
                .to_string();
            validate_stage_relative_path(&rel).map_err(|e| GuestFail::new(code::PATH_ESCAPE, e))?;
            if !seen.insert(rel.clone()) {
                continue; // duplicate request gathers once
            }
            let root_fd = self.root_fd(root);
            let (file, st) = open_regular_beneath(root_fd, &rel)?;
            let digest = hash_fd(file.as_raw_fd(), st.st_size as u64)?;
            total = total
                .checked_add(st.st_size as u64)
                .ok_or_else(|| GuestFail::new(code::BODY_TOO_LARGE, "export total overflow"))?;
            if total > MAX_STAGED_TOTAL_BYTES {
                return Err(GuestFail::new(
                    code::BODY_TOO_LARGE,
                    "export exceeds staged cap",
                ));
            }
            pinned.insert(
                rel.clone(),
                Pin {
                    dev: st.st_dev,
                    ino: st.st_ino,
                    size: st.st_size as u64,
                    mtime: st.st_mtime,
                },
            );
            files_out.push(StageFile {
                relative_path: rel,
                sha256: digest,
                size: st.st_size as u64,
                mime: "application/octet-stream".into(),
                provenance: None,
            });
        }
        if files_out.len() > MAX_STAGED_FILES {
            return Err(GuestFail::new(
                code::BODY_TOO_LARGE,
                "too many export files",
            ));
        }
        files_out.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
        let mut manifest = StageManifest {
            stage_id: stage_id.clone(),
            direction: StageDirection::Export,
            files: files_out,
            total_bytes: total,
            // placeholder; the canonical digest zeroes this field anyway
            manifest_digest: ArtifactDigest::of_bytes(b"placeholder"),
        };
        manifest.manifest_digest =
            agentkit_sandbox_contracts::canonical::manifest_digest_of(&manifest)
                .map_err(|_| GuestFail::new(code::IO, "manifest canonicalization"))?;
        let mut st = self
            .staging
            .lock()
            .map_err(|_| GuestFail::new(code::IO, "staging state poisoned; restart guest"))?;
        st.export = Some(ExportSession {
            stage_id: stage_id.clone(),
            manifest: manifest.clone(),
            pinned,
        });
        Ok(GuestOpOutcome::ExportOk { manifest })
    }

    pub(crate) fn export_fetch(
        &self,
        stage_id: &StageId,
        relative_path: &str,
        sequence: u32,
    ) -> Result<GuestOpOutcome, GuestFail> {
        let st = self
            .staging
            .lock()
            .map_err(|_| GuestFail::new(code::IO, "staging state poisoned; restart guest"))?;
        let Some(session) = st.export.as_ref() else {
            return Err(GuestFail::new(code::NOT_STARTED, "no gathered export"));
        };
        if &session.stage_id != stage_id {
            return Err(GuestFail::new(code::BAD_INPUT, "stage id mismatch"));
        }
        let spec = session
            .manifest
            .files
            .iter()
            .find(|f| f.relative_path == relative_path)
            .ok_or_else(|| GuestFail::new(code::NOT_FOUND, "path not in export manifest"))?;
        let pin = session
            .pinned
            .get(relative_path)
            .ok_or_else(|| GuestFail::new(code::NOT_FOUND, "unpinned export path"))?;
        let root_fd = self.root_fd(agentkit_sandbox_contracts::path::GuestRoot::Workspace);
        let (file, fst) = open_regular_beneath(root_fd, relative_path)?;
        if !pins_match(&fst, pin) {
            return Err(GuestFail::new(
                code::DIGEST_MISMATCH,
                "file mutated after gather",
            ));
        }
        let size = spec.size as usize;
        let start = (sequence as usize).saturating_mul(MAX_STAGE_CHUNK_BYTES);
        if start > size {
            return Err(GuestFail::new(code::BAD_INPUT, "sequence past end"));
        }
        let want = (size - start).min(MAX_STAGE_CHUNK_BYTES);
        let mut chunk = vec![0u8; want];
        let mut got = 0usize;
        while got < want {
            // SAFETY: pread into a valid buffer at tracked offset.
            let n = unsafe {
                libc::pread(
                    file.as_raw_fd(),
                    chunk[got..].as_mut_ptr().cast(),
                    want - got,
                    (start + got) as i64,
                )
            };
            if n < 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(GuestFail::io("pread", &e));
            }
            let n = usize::try_from(n).unwrap_or(0);
            if n == 0 {
                break; // shrank mid-read — worker digest check fails closed
            }
            got += n;
        }
        chunk.truncate(got);
        let last = start + got >= size;
        Ok(GuestOpOutcome::ExportDataOk {
            bytes_b64: b64(&chunk),
            chunk_digest: ArtifactDigest::of_bytes(&chunk),
            last,
        })
    }
}
