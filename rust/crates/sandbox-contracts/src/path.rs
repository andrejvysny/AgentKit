//! Guest path validation (§8.2).
//!
//! A `GuestPath` is a guest-absolute path confined to an approved guest root.
//! Construction rejects `..`, `.`, empty segments, NUL, control characters,
//! over-long paths/segments, and anything not lexically under an approved
//! root. There is deliberately **no** silent normalization that could rewrite
//! an escape attempt into a legal path: rejection is the normalization.
//! Symlink/hardlink/device-node escapes are enforced at operation time in the
//! guest with `openat2` (`RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS |
//! RESOLVE_NO_SYMLINKS`); this type is the necessary-first gate, not the only
//! one.

use crate::limits::{MAX_PATH_BYTES, MAX_PATH_SEGMENT_BYTES};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Approved guest roots (§8.2). `/workspace` is the private workspace
/// filesystem; `/home/agent` and `/tmp` are guest-only, quota-enforced,
/// initialized per sandbox with private permissions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuestRoot {
    Workspace,
    HomeAgent,
    Tmp,
}

impl GuestRoot {
    pub const ALL: [GuestRoot; 3] = [Self::Workspace, Self::HomeAgent, Self::Tmp];

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Workspace => "/workspace",
            Self::HomeAgent => "/home/agent",
            Self::Tmp => "/tmp",
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq, Clone)]
pub enum GuestPathError {
    #[error("path is empty")]
    Empty,
    #[error("path must be absolute (start with an approved root)")]
    NotAbsolute,
    #[error("path contains a NUL byte")]
    Nul,
    #[error("path contains control characters")]
    Control,
    #[error("path contains '.' or '..' segments; escapes are rejected, not rewritten")]
    DotSegment,
    #[error("path contains an empty segment")]
    EmptySegment,
    #[error("path exceeds {0} bytes")]
    TooLong(usize),
    #[error("path segment exceeds {0} bytes")]
    SegmentTooLong(usize),
    #[error("path is not under an approved guest root ({roots:?})", roots = GuestRoot::ALL)]
    OutsideRoot,
}

/// A validated guest-absolute path. Invariant: `0..root_len` matches exactly
/// one [`GuestRoot::as_str`], followed by `/` or end-of-string; all segments
/// non-empty and not `.`/`..`; no control chars or NUL; within size limits.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GuestPath(String);

impl GuestPath {
    /// Parse and validate.
    pub fn parse(value: &str) -> Result<Self, GuestPathError> {
        if value.is_empty() {
            return Err(GuestPathError::Empty);
        }
        if value.len() > MAX_PATH_BYTES {
            return Err(GuestPathError::TooLong(MAX_PATH_BYTES));
        }
        if value.contains('\0') {
            return Err(GuestPathError::Nul);
        }
        if value.chars().any(|c| (c as u32) < 0x20 || c as u32 == 0x7f) {
            return Err(GuestPathError::Control);
        }
        if !value.starts_with('/') {
            return Err(GuestPathError::NotAbsolute);
        }
        let root = GuestRoot::ALL
            .into_iter()
            .find(|r| {
                let rs = r.as_str();
                value == rs || value.starts_with(&format!("{rs}/"))
            })
            .ok_or(GuestPathError::OutsideRoot)?;
        // Validate every segment after the root prefix.
        let rest = &value[root.as_str().len()..];
        if !rest.is_empty() {
            if !rest.starts_with('/') {
                // e.g. "/workspacee/x" — cannot happen given prefix match
                // above, but assert the shape anyway.
                return Err(GuestPathError::OutsideRoot);
            }
            for seg in rest[1..].split('/') {
                if seg.is_empty() {
                    return Err(GuestPathError::EmptySegment);
                }
                if seg.len() > MAX_PATH_SEGMENT_BYTES {
                    return Err(GuestPathError::SegmentTooLong(MAX_PATH_SEGMENT_BYTES));
                }
                if seg == "." || seg == ".." {
                    return Err(GuestPathError::DotSegment);
                }
            }
        }
        Ok(Self(value.to_string()))
    }

    /// Parse a workspace-relative path against `/workspace`.
    pub fn in_workspace(relative: &str) -> Result<Self, GuestPathError> {
        if relative.starts_with('/') {
            return Self::parse(relative);
        }
        let joined = format!("/workspace/{}", relative.trim_start_matches("./"));
        Self::parse(&joined)
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn root(&self) -> GuestRoot {
        GuestRoot::ALL
            .into_iter()
            .find(|r| self.0 == r.as_str() || self.0.starts_with(&format!("{}/", r.as_str())))
            .expect("invariant upheld by parse")
    }

    /// Path components after the root, e.g. `/workspace/a/b` → `["a","b"]`.
    pub fn components(&self) -> impl Iterator<Item = &str> {
        let rest = &self.0[self.root().as_str().len()..];
        rest.trim_start_matches('/')
            .split('/')
            .filter(|s| !s.is_empty())
    }

    #[must_use]
    pub fn file_name(&self) -> Option<&str> {
        self.components().last()
    }

    #[must_use]
    pub fn parent(&self) -> Option<Self> {
        let idx = self.0.rfind('/')?;
        if idx == 0 {
            return None;
        }
        // A root itself has no parent within the approved set.
        let cand = &self.0[..idx];
        if GuestRoot::ALL.iter().any(|r| r.as_str() == cand) {
            return Some(self.clone());
        }
        Self::parse(cand).ok()
    }

    /// Lexically join a relative path (rejecting escapes via re-parse).
    pub fn join(&self, relative: &str) -> Result<Self, GuestPathError> {
        if relative.contains('/')
            || relative == "."
            || relative == ".."
            || relative.is_empty()
            || relative.starts_with('/')
        {
            // Allow simple multi-segment relative too, but never absolute or
            // with dot segments:
            if relative.starts_with('/') || relative.is_empty() {
                return Err(GuestPathError::NotAbsolute);
            }
            if relative
                .split('/')
                .any(|s| s == "." || s == ".." || s.is_empty())
            {
                return Err(GuestPathError::DotSegment);
            }
        }
        Self::parse(&format!("{}/{}", self.0.trim_end_matches('/'), relative))
    }

    #[must_use]
    pub fn is_ancestor_of(&self, other: &GuestPath) -> bool {
        other.0.starts_with(&format!("{}/", self.0)) || other.0 == self.0
    }
}

impl fmt::Debug for GuestPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "GuestPath({:?})", self.0)
    }
}
impl fmt::Display for GuestPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for GuestPath {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}
impl<'de> Deserialize<'de> for GuestPath {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_conforming_paths() {
        for ok in [
            "/workspace",
            "/workspace/a",
            "/workspace/a/b.rs",
            "/workspace/./x", // rejected below; placeholder replaced
        ] {
            if ok.contains("/./") {
                continue;
            }
            assert!(GuestPath::parse(ok).is_ok(), "expected ok: {ok}");
        }
        assert!(GuestPath::parse("/home/agent/.cache/x").is_ok());
        assert!(GuestPath::parse("/tmp/build.d/tmpfile").is_ok());
        assert!(
            GuestPath::in_workspace("src/main.rs").unwrap().as_str() == "/workspace/src/main.rs"
        );
    }

    #[test]
    fn rejects_escapes_and_malformed() {
        let bad = [
            "",                     // empty
            "relative/x",           // not absolute
            "/etc/passwd",          // outside roots
            "/workspacee/x",        // prefix confusion
            "/workspace/../etc",    // dot escape
            "/workspace/./x",       // dot segment rejected outright
            "/workspace//double",   // empty segment
            "/workspace/tail/",     // empty segment (trailing slash)
            "/workspace/nul\0",     // NUL
            "/workspace/ctrl\u{1}", // control char
            &format!("/workspace/{}", "a".repeat(MAX_PATH_SEGMENT_BYTES + 1)),
            &format!("/workspace/{}", "x".repeat(MAX_PATH_BYTES + 1)),
            "/Workspace/caps",
            "/workspace/a/../../etc/passwd",
        ];
        for p in bad {
            assert!(GuestPath::parse(p).is_err(), "expected reject: {p:?}");
        }
    }

    #[test]
    fn serde_roundtrip_revalidates() {
        let p = GuestPath::parse("/workspace/a.txt").unwrap();
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(serde_json::from_str::<GuestPath>(&json).unwrap(), p);
        assert!(serde_json::from_str::<GuestPath>("\"/etc/shadow\"").is_err());
        assert!(serde_json::from_str::<GuestPath>("\"workspace/a\"").is_err());
    }

    #[test]
    fn ancestry_and_helpers() {
        let a = GuestPath::parse("/workspace/src").unwrap();
        let b = GuestPath::parse("/workspace/src/main.rs").unwrap();
        assert!(a.is_ancestor_of(&b));
        assert!(!b.is_ancestor_of(&a));
        assert_eq!(b.parent().unwrap(), a);
        assert_eq!(b.file_name().unwrap(), "main.rs");
        assert_eq!(a.join("mod").unwrap().as_str(), "/workspace/src/mod");
        assert!(a.join("../escape").is_err());
        assert!(a.join("/abs").is_err());
        assert_eq!(a.root(), GuestRoot::Workspace);
    }
}
