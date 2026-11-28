//! The launcher's directories:
//!
//! - the data root and the socket root: created with mode 0700 when
//!   absent; each must then be a directory (not a link) owned by this user
//!   that grants group and others nothing. Only the leaf is checked, a
//!   restriction the configuration documents: macOS's per-user `$TMPDIR`
//!   lies below the `/var` link, so the ancestors cannot be required to be
//!   plain private directories, and they must instead be writable by this
//!   user and root only. The roots grant nobody else entry, so nothing
//!   inside them can be replaced by another user;
//! - `<socket_root>/<machine_dir_name(ws, gen)>`, mode 0700, where the
//!   adapter binds a machine's control and egress sockets and the helper
//!   (the same user) connects to them. No guest sees any host directory.
//!
//! Removal unlinks a machine directory's direct entries and removes it; it
//! never recurses.

use std::fs::{DirBuilder, Metadata};
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};

use agentkit_sandbox_proto::machine_dir_name;

use crate::naming::WorkspaceId;

/// Mode of every directory the launcher creates.
const DIR_MODE: u32 = 0o700;
/// Most entries removed from one machine directory.
const ENTRIES_MAX: usize = 64;

/// Check the leaf of `root`: a directory, not a link, owned by `own_uid`,
/// no group or other bits. Created (with its parents) mode 0700 when
/// absent; an existing one is checked as it is, never changed.
pub(crate) fn prepare_root(root: &Path, own_uid: u32) -> io::Result<()> {
    match std::fs::symlink_metadata(root) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            agentkit_sys::create_private_dir(root)?;
        }
        Err(error) => return Err(error),
        Ok(_) => {}
    }
    check_private(&std::fs::symlink_metadata(root)?, own_uid, root)
}

/// `meta` (read without following a link) is a private directory of
/// `own_uid`.
pub(crate) fn check_private(meta: &Metadata, own_uid: u32, path: &Path) -> io::Result<()> {
    if !meta.file_type().is_dir() {
        return Err(refused(path, "is not a directory (or is a link)"));
    }
    if meta.uid() != own_uid {
        return Err(refused(path, "is not owned by this user"));
    }
    if meta.mode() & 0o077 != 0 {
        return Err(refused(path, "grants access to group or others"));
    }
    Ok(())
}

/// Create the private directory `path` below an already checked root;
/// an existing one must pass [`check_private`].
pub(crate) fn private_child(path: &Path, own_uid: u32) -> io::Result<()> {
    match DirBuilder::new().mode(DIR_MODE).create(path) {
        Err(error) if error.kind() != io::ErrorKind::AlreadyExists => return Err(error),
        _ => {}
    }
    check_private(&std::fs::symlink_metadata(path)?, own_uid, path)
}

/// The machine socket directories below the socket root.
#[derive(Debug, Clone)]
pub(crate) struct SocketDirs {
    root: PathBuf,
    own_uid: u32,
}

impl SocketDirs {
    pub(crate) fn new(root: PathBuf, own_uid: u32) -> Self {
        Self { root, own_uid }
    }

    /// `<root>/<machine_dir_name(ws, generation)>`.
    pub(crate) fn path(&self, ws: &str, generation: u64) -> PathBuf {
        self.root.join(machine_dir_name(ws, generation))
    }

    /// A fresh, empty machine directory for `ws` at `generation`.
    pub(crate) fn prepare(&self, ws: &WorkspaceId, generation: u64) -> io::Result<PathBuf> {
        let path = self.path(ws.as_str(), generation);
        remove_flat(&path)?;
        private_child(&path, self.own_uid)?;
        Ok(path)
    }

    /// Remove the machine directory of `ws` at `generation` and the sockets
    /// in it. Absent is success.
    pub(crate) fn remove(&self, ws: &str, generation: u64) -> io::Result<()> {
        remove_flat(&self.path(ws, generation))
    }
}

/// Unlink every non-directory entry of `dir`, then remove it. A link at
/// `dir` is unlinked, never followed; a directory inside it is left alone,
/// so the removal then fails.
fn remove_flat(dir: &Path) -> io::Result<()> {
    let meta = match std::fs::symlink_metadata(dir) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        other => other?,
    };
    if !meta.file_type().is_dir() {
        return std::fs::remove_file(dir);
    }
    for entry in std::fs::read_dir(dir)?.take(ENTRIES_MAX) {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            std::fs::remove_file(entry.path())?;
        }
    }
    std::fs::remove_dir(dir)
}

pub(crate) fn refused(path: &Path, why: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("{} {why}", path.display()),
    )
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn uid() -> u32 {
        agentkit_sys::effective_uid().unwrap()
    }

    #[test]
    fn roots_are_created_private_and_links_or_open_modes_are_refused() {
        let scratch = tempfile::tempdir().unwrap();
        let root = scratch.path().join("a/b");
        prepare_root(&root, uid()).unwrap();
        let mode = std::fs::metadata(&root).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        let link = scratch.path().join("link");
        std::os::unix::fs::symlink(&root, &link).unwrap();
        assert!(check_private(&std::fs::symlink_metadata(&link).unwrap(), uid(), &link).is_err());
        let open = scratch.path().join("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o750)).unwrap();
        assert!(private_child(&open, uid()).is_err());
        assert!(
            prepare_root(&open, uid()).is_err(),
            "never loosened or fixed"
        );
        assert!(prepare_root(&link, uid()).is_err());
        assert!(prepare_root(&root, uid() + 1).is_err(), "another owner");
    }

    #[test]
    fn machine_directories_are_fresh_and_removed_flat() {
        let scratch = tempfile::tempdir().unwrap();
        let dirs = SocketDirs::new(scratch.path().to_path_buf(), uid());
        let ws = WorkspaceId::parse("ws").unwrap();
        let dir = dirs.prepare(&ws, 3).unwrap();
        std::fs::write(dir.join("control.sock"), b"").unwrap();
        assert_eq!(dirs.prepare(&ws, 3).unwrap(), dir);
        assert!(!dir.join("control.sock").exists(), "prepare empties it");
        std::fs::write(dir.join("egress.sock"), b"").unwrap();
        dirs.remove("ws", 3).unwrap();
        assert!(!dir.exists());
        dirs.remove("ws", 3).unwrap();
    }
}
