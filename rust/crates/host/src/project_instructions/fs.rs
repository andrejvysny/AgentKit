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
//! Platform limit: the `O_NOFOLLOW` open and the `dev`/`ino` identity check
//! are POSIX; on non-Unix platforms every read is a refusal (skip with a
//! warning), never an unchecked read.

use async_trait::async_trait;

use super::{ProjectInstructionsSource, ProjectInstructionsText};
use crate::{HostError, HostResult};

/// Reads `AGENTS.md` / `CLAUDE.md` from a workspace root through a
/// symlink-refusing, root-confining open (Unix only; see the module docs).
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
        #[cfg(not(unix))]
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
    use std::io::{ErrorKind, Read};
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
    // One byte past the cap is the truncation witness (Node reads maxBytes+1).
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
    Ok(Some(ProjectInstructionsText { text, truncated }))
}
