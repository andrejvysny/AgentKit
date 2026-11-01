//! The secure filesystem instruction source for the binding: a port of
//! `packages/runner-local/src/project-instructions.ts`.
//!
//! Confinement mirrors the skill source: the root is resolved with
//! `realpath` (a symlinked ROOT is the caller's explicit choice), but the
//! FILE must be a regular file directly under it and never a symlink — a
//! repository can ship a link named `AGENTS.md` pointing at `~/.ssh/config`,
//! and its text would reach the model. `O_NOFOLLOW` plus the lstat/fstat
//! dev/ino identity check closes the swap between the check and the open.
//!
//! A missing file is `Ok(None)`. A refusal (symlink, escape, special file)
//! is an `Err`, so the caller can tell "no instructions" from "instructions
//! withheld". Oversize files are truncated to `max_bytes`, not refused: a
//! long AGENTS.md still carries useful rules in its first 64 KiB.
//!
//! Platform limit: POSIX uses `O_NOFOLLOW` plus the `dev`/`ino` identity
//! check; Windows refuses reparse points up front and verifies the opened
//! handle's volume-serial/file-index identity (see `read_plain_file` below).
//! On any other platform every read is a refusal (a hard error), never an
//! unchecked read.

use async_trait::async_trait;

use super::{ProjectInstructionsSource, ProjectInstructionsText};
use crate::{HostError, HostResult};

/// Reads `AGENTS.md` / `CLAUDE.md` from a workspace root through a
/// symlink-refusing, root-confining open (POSIX and Windows; see the module
/// docs).
#[derive(Clone, Copy, Debug, Default)]
pub struct FileProjectInstructions;

#[async_trait]
impl ProjectInstructionsSource for FileProjectInstructions {
    async fn read(
        &self,
        root: &str,
        name: &str,
        max_bytes: usize,
    ) -> HostResult<Option<ProjectInstructionsText>> {
        if name.is_empty()
            || name == "."
            || name == ".."
            || name.contains('/')
            || name.contains('\\')
            || name.contains('\0')
        {
            return Err(HostError::Execution(
                "Instruction file name must be a plain file name.".into(),
            ));
        }
        #[cfg(unix)]
        {
            let (root, name) = (root.to_owned(), name.to_owned());
            tokio::task::spawn_blocking(move || read_plain_file(&root, &name, max_bytes))
                .await
                .map_err(|_| HostError::Execution("Instruction reader stopped.".into()))?
        }
        #[cfg(windows)]
        {
            let (root, name) = (root.to_owned(), name.to_owned());
            tokio::task::spawn_blocking(move || read_plain_file(&root, &name, max_bytes))
                .await
                .map_err(|_| HostError::Execution("Instruction reader stopped.".into()))?
        }
        #[cfg(all(not(unix), not(windows)))]
        {
            let _ = (root, max_bytes);
            Err(HostError::Execution(
                "Project instructions are not read on this platform.".into(),
            ))
        }
    }
}

#[cfg(unix)]
fn read_plain_file(
    root: &str,
    name: &str,
    max_bytes: usize,
) -> HostResult<Option<ProjectInstructionsText>> {
    use std::fs::OpenOptions;
    use std::io::ErrorKind;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

    // A symlinked root is the caller's explicit choice: resolve it once.
    let real_root = std::fs::canonicalize(root)
        .map_err(|error| HostError::Execution(format!("Cannot resolve workspace root: {error}")))?;
    let target = real_root.join(name);
    let expected = match std::fs::symlink_metadata(&target) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(HostError::Execution(format!(
                "Cannot inspect instruction file: {error}"
            )));
        }
    };
    if expected.file_type().is_symlink() {
        return Err(HostError::Execution(
            "Instruction file is a symlink.".into(),
        ));
    }
    if !expected.is_file() {
        return Err(HostError::Execution(
            "Instruction file is not a regular file.".into(),
        ));
    }
    // O_NOFOLLOW refuses a swap to a symlink; O_NONBLOCK refuses a FIFO.
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&target)
        .map_err(|error| HostError::Execution(format!("Cannot open instruction file: {error}")))?;
    // fstat the OPEN file: same dev/ino or the check raced a replacement.
    let opened = file
        .metadata()
        .map_err(|error| HostError::Execution(format!("Cannot stat instruction file: {error}")))?;
    if !opened.is_file() || opened.dev() != expected.dev() || opened.ino() != expected.ino() {
        return Err(HostError::Execution(
            "Instruction file changed during open.".into(),
        ));
    }
    Ok(Some(drain_and_decode(&mut file, max_bytes)?))
}

/// Windows reads the same two ways the POSIX branch does — refuse the
/// symlink up front, then verify the OPEN file is the inspected file — with
/// the platform's own primitives: `symlink_metadata` opens with
/// `FILE_FLAG_OPEN_REPARSE_POINT` (a link or junction is seen as itself),
/// and the identity check compares the volume serial + file index that the
/// handle itself reports via `GetFileInformationByHandle` — the
/// `fstat`/`dev`/`ino` equivalent (see [`file_identity`]; the `std` getters
/// for these fields are unstable, so the check goes through `windows-sys`).
/// There is no `O_NOFOLLOW`, so a swap between the check and the open is
/// caught by the identity mismatch — the opened handle then names a
/// different file and the read is refused, never followed blindly.
#[cfg(windows)]
fn read_plain_file(
    root: &str,
    name: &str,
    max_bytes: usize,
) -> HostResult<Option<ProjectInstructionsText>> {
    use std::fs::OpenOptions;
    use std::io::ErrorKind;
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;

    let real_root = std::fs::canonicalize(root)
        .map_err(|error| HostError::Execution(format!("Cannot resolve workspace root: {error}")))?;
    let target = real_root.join(name);
    let expected = match std::fs::symlink_metadata(&target) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(HostError::Execution(format!(
                "Cannot inspect instruction file: {error}"
            )));
        }
    };
    if expected.file_type().is_symlink() {
        return Err(HostError::Execution(
            "Instruction file is a symlink.".into(),
        ));
    }
    if !expected.is_file() {
        return Err(HostError::Execution(
            "Instruction file is not a regular file.".into(),
        ));
    }
    // `lstat` half of the identity check: query the volume serial + file
    // index of the entry itself. The reparse-point flag means the handle is
    // the path entry we just inspected — never the file a link points at —
    // mirroring what `lstat` sees on POSIX.
    let inspected = OpenOptions::new()
        .read(true)
        .attributes(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(&target)
        .map_err(|error| {
            HostError::Execution(format!("Cannot inspect instruction file: {error}"))
        })?;
    let expected_identity = file_identity(&inspected)?;
    drop(inspected);
    // `O_NOFOLLOW` has no Windows equivalent, so the swap between the check
    // and the open is caught below instead: the opened handle follows any
    // link, and an identity mismatch then names a different file.
    let mut file = OpenOptions::new()
        .read(true)
        .open(&target)
        .map_err(|error| HostError::Execution(format!("Cannot open instruction file: {error}")))?;
    let opened = file
        .metadata()
        .map_err(|error| HostError::Execution(format!("Cannot stat instruction file: {error}")))?;
    if !opened.is_file() || file_identity(&file)? != expected_identity {
        return Err(HostError::Execution(
            "Instruction file changed during open.".into(),
        ));
    }
    Ok(Some(drain_and_decode(&mut file, max_bytes)?))
}

/// The volume serial + file index the system reports FOR THE OPEN HANDLE —
/// the `fstat`/`dev`/`ino` equivalent. Queried through
/// `GetFileInformationByHandle` (stable `windows-sys` binding) because the
/// matching `std::os::windows::fs::MetadataExt` getters are unstable behind
/// `windows_by_handle`, which the pinned stable toolchain rejects outright.
#[cfg(windows)]
fn file_identity(handle: &impl std::os::windows::io::AsRawHandle) -> HostResult<(u32, u64)> {
    use std::mem::MaybeUninit;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };

    let mut info = MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
    // SAFETY: `handle` is an open handle owned by the caller, and the
    // out-param points at writable memory of the exact struct size.
    let reported = unsafe { GetFileInformationByHandle(handle.as_raw_handle(), info.as_mut_ptr()) };
    if reported == 0 {
        return Err(HostError::Execution(format!(
            "Cannot identify instruction file: {}",
            std::io::Error::last_os_error()
        )));
    }
    // SAFETY: a successful call fills the whole struct.
    let info = unsafe { info.assume_init() };
    Ok((
        info.dwVolumeSerialNumber,
        (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
    ))
}

/// Read `max_bytes + 1` (the truncation witness, mirroring Node's
/// `maxBytes + 1`) and decode like `Buffer.toString("utf8")`.
fn drain_and_decode<R: std::io::Read>(
    file: &mut R,
    max_bytes: usize,
) -> HostResult<ProjectInstructionsText> {
    use std::io::ErrorKind;

    let mut buffer = vec![0u8; max_bytes + 1];
    let mut total = 0usize;
    while total < buffer.len() {
        match file.read(&mut buffer[total..]) {
            Ok(0) => break,
            Ok(read) => total += read,
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => {
                return Err(HostError::Execution(format!(
                    "Cannot read instruction file: {error}"
                )));
            }
        }
    }
    let truncated = total > max_bytes;
    // Node's `toString("utf8")`: every undecodable byte becomes U+FFFD, so a
    // cut inside a multi-byte character decodes to a trailing U+FFFD — drop
    // exactly one, only on the truncated path.
    let mut text = String::from_utf8_lossy(&buffer[..total.min(max_bytes)]).into_owned();
    if truncated && let Some(stripped) = text.strip_suffix('\u{FFFD}') {
        text.truncate(stripped.len());
    }
    Ok(ProjectInstructionsText { text, truncated })
}
