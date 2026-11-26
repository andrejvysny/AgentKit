//! Paths below `/workspace` and `/home/agent`, resolved one component at
//! a time from the root's descriptor without following links (ADR 0039):
//! the workload can write both trees, so any component may be a link
//! planted to redirect the supervisor. A link anywhere on the way is
//! refused, never followed; the last step of every operation is a `*_at`
//! call on the descriptor this walk returns.

use std::fs::File;
use std::io;
use std::path::Path;

use agentkit_sandbox_proto::guest::{GuestErrorCode, Reply};
use agentkit_sandbox_proto::validate::PATH_MAX_BYTES;
use agentkit_sys::{EntryType, create_dir_at, open_dir_at_nofollow, open_dir_nofollow};

use crate::conn::error_reply;

/// Longest path component in bytes (Linux `NAME_MAX`).
const NAME_MAX_BYTES: usize = 255;
/// Mode of directories the supervisor creates (the umask applies).
pub(crate) const DIR_MODE: u32 = 0o755;

/// Why a path operation failed, as the guest reports it.
#[derive(Debug)]
pub(crate) enum FsError {
    /// The path is not a normalised relative path.
    Invalid,
    /// A component or the target does not exist.
    NotFound,
    /// A component that must be a directory is not one.
    NotADirectory,
    /// The target is a directory, link or special file.
    NotARegularFile,
    /// A component is a link.
    Link,
    /// The file is larger than the request allows.
    TooLarge { size: u64 },
    /// The volume is full.
    NoSpace,
    /// Anything else.
    Io(io::Error),
}

impl FsError {
    /// Classify an error of a step that names no entry kind.
    pub(crate) fn from_io(error: io::Error) -> Self {
        match error.kind() {
            io::ErrorKind::NotFound => FsError::NotFound,
            io::ErrorKind::StorageFull => FsError::NoSpace,
            _ => FsError::Io(error),
        }
    }

    /// The guest error code for this failure.
    pub(crate) fn code(&self) -> GuestErrorCode {
        match self {
            FsError::Invalid | FsError::Link => GuestErrorCode::InvalidPath,
            FsError::NotFound => GuestErrorCode::NotFound,
            FsError::NotADirectory => GuestErrorCode::NotADirectory,
            FsError::NotARegularFile => GuestErrorCode::NotARegularFile,
            FsError::TooLarge { size } => GuestErrorCode::FileTooLarge { size: *size },
            FsError::NoSpace => GuestErrorCode::NoSpace,
            FsError::Io(_) => GuestErrorCode::Internal,
        }
    }

    /// The error reply for this failure. The message names the class, not
    /// the path, so it carries nothing the workload chose.
    pub(crate) fn reply(&self) -> Reply {
        let message = match self {
            FsError::Invalid => "path is not a normalised relative path".to_owned(),
            FsError::Link => "path crosses a symbolic link".to_owned(),
            FsError::NotFound => "path does not exist".to_owned(),
            FsError::NotADirectory => "a path component is not a directory".to_owned(),
            FsError::NotARegularFile => "target is not a regular file".to_owned(),
            FsError::TooLarge { size } => format!("file is {size} bytes, over the limit"),
            FsError::NoSpace => "the volume is full".to_owned(),
            FsError::Io(error) => format!("file operation failed: {error}"),
        };
        error_reply(self.code(), message)
    }
}

/// The components of a normalised relative path: `""` is the root (no
/// components); a leading `/`, an empty, `.` or `..` component, a NUL, a
/// component over 255 bytes or a path over 4096 bytes is refused.
pub(crate) fn components(path: &str) -> Result<Vec<&str>, FsError> {
    if path.is_empty() {
        return Ok(Vec::new());
    }
    if path.len() > PATH_MAX_BYTES || path.contains('\0') {
        return Err(FsError::Invalid);
    }
    let parts: Vec<&str> = path.split('/').collect();
    let bad = |part: &&str| {
        part.is_empty() || *part == "." || *part == ".." || part.len() > NAME_MAX_BYTES
    };
    if parts.iter().any(bad) {
        return Err(FsError::Invalid);
    }
    Ok(parts)
}

/// Open a root directory itself (a mount point the launcher made).
pub(crate) fn open_root(root: &Path) -> Result<File, FsError> {
    open_dir_nofollow(root).map_err(FsError::from_io)
}

/// Descend from `dir` through `names`, refusing links. With `create`, a
/// missing directory is made (mode [`DIR_MODE`]) and entered.
pub(crate) fn walk(dir: File, names: &[&str], create: bool) -> Result<File, FsError> {
    let mut current = dir;
    for name in names {
        current = step(&current, name, create.then_some(DIR_MODE))?;
    }
    Ok(current)
}

/// Enter the directory `name` below `dir`, refusing a link. With
/// `make`, a missing directory is made with that mode and entered.
pub(crate) fn step(dir: &File, name: &str, make: Option<u32>) -> Result<File, FsError> {
    match (open_dir_at_nofollow(dir, name), make) {
        (Ok(next), _) => Ok(next),
        (Err(error), Some(mode)) if error.kind() == io::ErrorKind::NotFound => {
            match create_dir_at(dir, name, mode) {
                Ok(()) => {}
                Err(made) if made.kind() == io::ErrorKind::AlreadyExists => {}
                Err(made) => return Err(classify(dir, name, made)),
            }
            open_dir_at_nofollow(dir, name).map_err(|again| classify(dir, name, again))
        }
        (Err(error), _) => Err(classify(dir, name, error)),
    }
}

/// What a failed step on `name` below `dir` means: the entry is looked at
/// again, without following it, to tell a link from a file.
pub(crate) fn classify(dir: &File, name: &str, error: io::Error) -> FsError {
    let fallback = FsError::from_io(error);
    if matches!(fallback, FsError::NotFound | FsError::NoSpace) {
        return fallback;
    }
    match agentkit_sys::stat_at_nofollow(dir, name) {
        Ok(stat) if stat.kind == EntryType::Symlink => FsError::Link,
        Ok(stat) if stat.kind != EntryType::Directory => FsError::NotADirectory,
        Err(again) if again.kind() == io::ErrorKind::NotFound => FsError::NotFound,
        _ => fallback,
    }
}

#[cfg(test)]
mod tests;
