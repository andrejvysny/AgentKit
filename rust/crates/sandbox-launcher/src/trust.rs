//! Directories the launcher's safety rests on: the socket host directory
//! (its machine directories become mount sources), the state directory and
//! the listen socket's directory.
//!
//! Each is opened from `/` one component at a time without following
//! links, and every directory on the way is checked through its own
//! descriptor (`fstat`), so the directory checked is the one used. An
//! ancestor must be owned by root or the launcher and not writable by
//! group or others; a sticky world-writable directory such as `/tmp` is
//! refused like any other. Otherwise a user who can write an ancestor could
//! rename the directory away and put one of their own, or a link, in its
//! place between the launcher's check and the runtime's (or the kernel's)
//! own lookup of the path.

use std::fs::{File, Metadata};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

/// Where the checked walk starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TrustRoot(PathBuf);

impl TrustRoot {
    /// `/`: every directory of a path is checked, `/` included.
    pub(crate) fn system() -> Self {
        Self(PathBuf::from("/"))
    }

    /// Check only from `root` down. Tests root their trees in a scratch
    /// directory whose own ancestors (`/tmp` on Linux) are world-writable.
    #[cfg(test)]
    pub(crate) fn at(root: &Path) -> Self {
        Self(root.to_path_buf())
    }

    /// Open the directory `dir`: every component from `/` without following
    /// links, and every directory from the trusted root down to `dir`'s
    /// parent checked as an ancestor ([`check_ancestor`]). `dir` itself is
    /// answered unchecked, for the caller's own rule.
    pub(crate) fn open(&self, dir: &Path, own_uid: u32) -> io::Result<File> {
        if !dir.starts_with(&self.0) {
            return Err(refused(dir, "is not below the trusted root"));
        }
        let mut current = agentkit_sys::open_dir_nofollow(Path::new("/"))?;
        let mut shown = PathBuf::from("/");
        for component in dir.components() {
            match component {
                Component::RootDir => continue,
                Component::Normal(name) => {
                    if shown.starts_with(&self.0) {
                        check_ancestor(&current.metadata()?, own_uid, &shown)?;
                    }
                    let name = name.to_str().ok_or_else(|| refused(dir, "is not UTF-8"))?;
                    current = agentkit_sys::open_dir_at_nofollow(&current, name)
                        .map_err(|error| with_path(error, &shown.join(name)))?;
                    shown.push(name);
                }
                _ => return Err(refused(dir, "is not a plain absolute path")),
            }
        }
        Ok(current)
    }

    /// Open `dir` and check it with the ancestor rule as well.
    pub(crate) fn open_checked(&self, dir: &Path, own_uid: u32) -> io::Result<File> {
        let file = self.open(dir, own_uid)?;
        check_ancestor(&file.metadata()?, own_uid, dir)?;
        Ok(file)
    }
}

/// An ancestor: owned by root or the launcher, not writable by group or
/// others.
pub(crate) fn check_ancestor(meta: &Metadata, own_uid: u32, path: &Path) -> io::Result<()> {
    if meta.uid() != 0 && meta.uid() != own_uid {
        return Err(refused(path, "is not owned by root or the launcher"));
    }
    if meta.mode() & 0o022 != 0 {
        return Err(refused(path, "is writable by group or others"));
    }
    Ok(())
}

/// The listen socket's directory: owned by the launcher itself and not
/// writable by group or others, so nobody else can replace the socket
/// between its `bind` and the mode change that follows by path.
pub(crate) fn check_private_parent(meta: &Metadata, own_uid: u32, path: &Path) -> io::Result<()> {
    if meta.uid() != own_uid {
        return Err(refused(path, "is not owned by the launcher"));
    }
    if meta.mode() & 0o022 != 0 {
        return Err(refused(path, "is writable by group or others"));
    }
    Ok(())
}

pub(crate) fn refused(path: &Path, why: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("{} {why}", path.display()),
    )
}

fn with_path(error: io::Error, path: &Path) -> io::Error {
    io::Error::new(error.kind(), format!("{}: {error}", path.display()))
}

#[cfg(test)]
mod tests;
