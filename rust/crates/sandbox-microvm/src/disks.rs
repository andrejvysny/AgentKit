//! Per-workspace files below `<data_root>/workspaces/<ws>/` (mode 0700):
//! `workspace.img` and `home.img`, raw sparse files the guest formats as
//! ext4 on first boot, plus the last machine's `console.log` and
//! `helper.log`. The directory is named by the checked workspace id, one
//! safe path component.
//!
//! - A disk is created sparse (`set_len`, mode 0600) when absent and never
//!   resized; growing one would need `resize2fs` in the guest.
//! - Usage is allocated bytes (`st_blocks * 512`), not the apparent size.
//! - `reset_home` recreates `home.img`; `destroy` removes the directory's
//!   files and the directory.

use std::fs::OpenOptions;
use std::io;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use crate::dirs::private_child;
use crate::naming::WorkspaceId;

const WORKSPACE_DISK: &str = "workspace.img";
const HOME_DISK: &str = "home.img";
const CONSOLE_LOG: &str = "console.log";
const HELPER_LOG: &str = "helper.log";
/// Mode of every file the launcher creates.
const FILE_MODE: u32 = 0o600;

/// The paths of one workspace's files.
#[derive(Debug, Clone)]
pub(crate) struct WorkspaceFiles {
    pub(crate) dir: PathBuf,
    pub(crate) workspace_disk: PathBuf,
    pub(crate) home_disk: PathBuf,
    pub(crate) console_log: PathBuf,
    pub(crate) helper_log: PathBuf,
}

/// The workspace directories below `<data_root>/workspaces`.
#[derive(Debug, Clone)]
pub(crate) struct Disks {
    root: PathBuf,
    own_uid: u32,
    workspace_bytes: u64,
    home_bytes: u64,
}

impl Disks {
    pub(crate) fn new(root: PathBuf, own_uid: u32, workspace_bytes: u64, home_bytes: u64) -> Self {
        Self {
            root,
            own_uid,
            workspace_bytes,
            home_bytes,
        }
    }

    /// The paths of `ws`'s files.
    pub(crate) fn files(&self, ws: &WorkspaceId) -> WorkspaceFiles {
        let dir = self.root.join(ws.as_str());
        WorkspaceFiles {
            workspace_disk: dir.join(WORKSPACE_DISK),
            home_disk: dir.join(HOME_DISK),
            console_log: dir.join(CONSOLE_LOG),
            helper_log: dir.join(HELPER_LOG),
            dir,
        }
    }

    /// Create the directory and both disks of `ws` where absent.
    pub(crate) fn prepare(&self, ws: &WorkspaceId) -> io::Result<WorkspaceFiles> {
        private_child(&self.root, self.own_uid)?;
        let files = self.files(ws);
        private_child(&files.dir, self.own_uid)?;
        create_sparse(&files.workspace_disk, self.workspace_bytes)?;
        create_sparse(&files.home_disk, self.home_bytes)?;
        Ok(files)
    }

    /// Allocated bytes of the workspace disk and of the home disk; an
    /// absent disk counts 0.
    pub(crate) fn usage(&self, ws: &WorkspaceId) -> io::Result<(u64, u64)> {
        let files = self.files(ws);
        Ok((
            allocated(&files.workspace_disk)?,
            allocated(&files.home_disk)?,
        ))
    }

    /// Replace the home disk of `ws` with a new empty one.
    pub(crate) fn reset_home(&self, ws: &WorkspaceId) -> io::Result<()> {
        let files = self.prepare(ws)?;
        remove_file(&files.home_disk)?;
        create_sparse(&files.home_disk, self.home_bytes)
    }

    /// Remove every file of `ws` and its directory. Absent is success.
    pub(crate) fn destroy(&self, ws: &WorkspaceId) -> io::Result<()> {
        let files = self.files(ws);
        for file in [
            &files.workspace_disk,
            &files.home_disk,
            &files.console_log,
            &files.helper_log,
        ] {
            remove_file(file)?;
        }
        match std::fs::remove_dir(&files.dir) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        }
    }
}

/// A new sparse file of `bytes` at `path`, unless one exists. Never
/// follows a link at `path`.
fn create_sparse(path: &Path, bytes: u64) -> io::Result<()> {
    let created = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(FILE_MODE)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path);
    match created {
        Ok(file) => {
            file.set_len(bytes)?;
            file.sync_all()
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let meta = std::fs::symlink_metadata(path)?;
            if meta.file_type().is_file() {
                Ok(())
            } else {
                Err(crate::dirs::refused(path, "is not a regular file"))
            }
        }
        Err(error) => Err(error),
    }
}

/// A log file for one helper run: truncated, mode 0600, never through a
/// link.
pub(crate) fn open_log(path: &Path) -> io::Result<std::fs::File> {
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(FILE_MODE)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

fn allocated(path: &Path) -> io::Result<u64> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => Ok(meta.blocks().saturating_mul(512)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error),
    }
}

fn remove_file(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Seek, SeekFrom, Write};

    use super::*;

    const MIB: u64 = 1024 * 1024;

    fn disks(root: &Path) -> Disks {
        let uid = agentkit_sys::effective_uid().unwrap();
        Disks::new(root.join("workspaces"), uid, 64 * MIB, 16 * MIB)
    }

    #[test]
    fn disks_are_sparse_counted_by_allocation_and_reset() {
        let scratch = tempfile::tempdir().unwrap();
        let disks = disks(scratch.path());
        let ws = WorkspaceId::parse("ws").unwrap();
        assert_eq!(disks.usage(&ws).unwrap(), (0, 0));
        let files = disks.prepare(&ws).unwrap();
        assert_eq!(
            std::fs::metadata(&files.workspace_disk).unwrap().len(),
            64 * MIB
        );
        let (workspace, home) = disks.usage(&ws).unwrap();
        assert!(workspace < MIB && home < MIB, "sparse: {workspace} {home}");
        let mut file = OpenOptions::new()
            .write(true)
            .open(&files.home_disk)
            .unwrap();
        file.seek(SeekFrom::Start(4 * MIB)).unwrap();
        file.write_all(&vec![7_u8; MIB as usize]).unwrap();
        file.sync_all().unwrap();
        assert!(disks.usage(&ws).unwrap().1 >= MIB);
        disks.reset_home(&ws).unwrap();
        assert!(disks.usage(&ws).unwrap().1 < MIB, "a fresh home disk");
        disks.destroy(&ws).unwrap();
        assert!(!files.dir.exists());
        assert_eq!(disks.usage(&ws).unwrap(), (0, 0));
    }

    #[test]
    fn a_link_in_place_of_a_disk_is_refused() {
        let scratch = tempfile::tempdir().unwrap();
        let disks = disks(scratch.path());
        let ws = WorkspaceId::parse("ws").unwrap();
        let files = disks.prepare(&ws).unwrap();
        std::fs::remove_file(&files.workspace_disk).unwrap();
        std::os::unix::fs::symlink("/etc/hosts", &files.workspace_disk).unwrap();
        assert!(disks.prepare(&ws).is_err());
    }
}
