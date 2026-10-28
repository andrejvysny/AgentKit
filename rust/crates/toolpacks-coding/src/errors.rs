//! User-facing tool failures: the tool turns them into `ok:false` results,
//! never into a throw that fails the run. Mirrors `packages/toolpacks-coding/src/errors.ts`.

/// A user-facing failure: the tool turns it into an `ok:false` result with the
/// code and message, never into a throw that fails the run.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct CodingError {
    pub code: String,
    pub message: String,
}

impl CodingError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_owned(),
            message: message.into(),
        }
    }
}

/// The session's last-read version no longer matches the file on disk.
pub fn file_changed_error(label: &str) -> CodingError {
    CodingError::new(
        "file_changed",
        format!(
            "{label} changed since this session last read it (another session or an external editor modified it). Read it again and redo the edit."
        ),
    )
}

/// write_file would overwrite an existing file this session never read.
pub fn file_not_read_error(label: &str, partial: bool) -> CodingError {
    CodingError::new(
        "file_not_read",
        if partial {
            format!(
                "{label} already exists and you only read part of it. Read the whole file (no offset/limit, nothing truncated) or pass expectedSha256 before overwriting it."
            )
        } else {
            format!("{label} already exists. Read it before overwriting it.")
        },
    )
}

/// write_file lost a create race: the file appeared before our link.
pub fn file_created_elsewhere_error(label: &str) -> CodingError {
    CodingError::new(
        "file_changed",
        format!(
            "{label} was created by someone else while writing it. Read it, then edit or overwrite it."
        ),
    )
}
