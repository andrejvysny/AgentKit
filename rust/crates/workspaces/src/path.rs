//! [`RelPath`]: a validated path relative to a root inside the guest
//! ([`PathRoot`]: `/workspace` unless a request names `/home/agent`). The
//! host never sends an absolute or escaping path to a backend; a model
//! argument that is one is refused before dispatch.

use std::fmt;

use serde::{Deserialize, Serialize};

/// The tree inside the guest a [`RelPath`] is resolved against. Both live
/// on the workspace volume; a path can never leave its root.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PathRoot {
    /// `/workspace`: the working tree (the default).
    #[default]
    Workspace,
    /// `/home/agent`: user-level installs and caches. `reset_home` empties
    /// it and leaves `/workspace` alone.
    Home,
}

impl PathRoot {
    /// The absolute guest directory, for messages.
    pub fn guest_dir(self) -> &'static str {
        match self {
            PathRoot::Workspace => "/workspace",
            PathRoot::Home => "/home/agent",
        }
    }
}

/// Longest accepted path in bytes, after normalisation (Linux `PATH_MAX`).
pub const REL_PATH_MAX_BYTES: usize = 4096;
/// Longest accepted path segment in bytes (Linux `NAME_MAX`).
pub const REL_PATH_SEGMENT_MAX_BYTES: usize = 255;

/// A normalised relative path: no leading `/`, no `.` or empty segments, no
/// `..` and no NUL byte. The empty path is the root itself and displays as
/// `.`. Construction resolves `a/../b` to `b` and refuses a `..` that would
/// climb above the root.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RelPath(String);

/// Why text is not an acceptable [`RelPath`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RelPathError {
    /// The path starts with `/`.
    #[error("path must be relative to the workspace, not absolute")]
    Absolute,
    /// A `..` segment climbs above the root.
    #[error("path escapes the workspace root")]
    EscapesRoot,
    /// The path contains a NUL byte.
    #[error("path contains a NUL byte")]
    Nul,
    /// The normalised path is longer than [`REL_PATH_MAX_BYTES`].
    #[error("path is {0} bytes, more than the limit")]
    TooLong(usize),
    /// One segment is longer than [`REL_PATH_SEGMENT_MAX_BYTES`].
    #[error("path segment is {0} bytes, more than the limit")]
    SegmentTooLong(usize),
}

impl RelPath {
    /// The root (`.`).
    pub fn root() -> Self {
        Self(String::new())
    }

    /// Validate and normalise `text`. `""` and `"."` are the root.
    pub fn parse(text: &str) -> Result<Self, RelPathError> {
        if text.contains('\0') {
            return Err(RelPathError::Nul);
        }
        if text.starts_with('/') {
            return Err(RelPathError::Absolute);
        }
        let mut segments: Vec<&str> = Vec::new();
        for segment in text.split('/') {
            match segment {
                "" | "." => {}
                ".." => {
                    segments.pop().ok_or(RelPathError::EscapesRoot)?;
                }
                _ if segment.len() > REL_PATH_SEGMENT_MAX_BYTES => {
                    return Err(RelPathError::SegmentTooLong(segment.len()));
                }
                _ => segments.push(segment),
            }
        }
        let joined = segments.join("/");
        if joined.len() > REL_PATH_MAX_BYTES {
            return Err(RelPathError::TooLong(joined.len()));
        }
        Ok(Self(joined))
    }

    /// True for the root.
    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    /// The normalised text; empty for the root.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The segments, outermost first; none for the root.
    pub fn segments(&self) -> impl Iterator<Item = &str> {
        self.0.split('/').filter(|segment| !segment.is_empty())
    }

    /// `other` resolved below `self` (for a chat folder plus a model path).
    pub fn join(&self, other: &RelPath) -> Result<RelPath, RelPathError> {
        if self.is_root() {
            return Ok(other.clone());
        }
        if other.is_root() {
            return Ok(self.clone());
        }
        RelPath::parse(&format!("{}/{}", self.0, other.0))
    }

    /// True when `self` is `ancestor` or lies below it.
    pub fn starts_with(&self, ancestor: &RelPath) -> bool {
        ancestor.is_root()
            || self.0 == ancestor.0
            || self
                .0
                .strip_prefix(ancestor.0.as_str())
                .is_some_and(|rest| rest.starts_with('/'))
    }
}

impl TryFrom<String> for RelPath {
    type Error = RelPathError;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::parse(&text)
    }
}

impl From<RelPath> for String {
    fn from(path: RelPath) -> Self {
        path.0
    }
}

impl fmt::Display for RelPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_root() {
            f.write_str(".")
        } else {
            f.write_str(&self.0)
        }
    }
}

#[cfg(test)]
mod tests;
