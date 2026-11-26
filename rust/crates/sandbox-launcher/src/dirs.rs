//! Per-machine socket directories: `<host>/<machine>`, where `<machine>`
//! is `agentkit_sandbox_proto::machine_dir_name(ws, generation)` (24
//! bytes, so socket paths stay short). A machine directory is the only
//! host path a container sees (read-only at `/run/agentkit`, as the mount
//! root, so the container never traverses `<host>`).
//!
//! - `<host>` and every directory above it are opened from the trusted
//!   root without following links and checked by descriptor
//!   ([`crate::trust`]), at startup and again at every `prepare`.
//! - `<host>` belongs to the launcher and grants others nothing: mode
//!   0710 with the server's group when the launcher and the server are
//!   different users (Docker: the launcher is root), 0700 when they are
//!   the same (rootless Podman). Other local users cannot reach a machine's
//!   sockets, and nobody but the launcher can create, rename or replace a
//!   machine directory between this check and the runtime's own lookup of
//!   the mount source.
//! - A machine directory is handed to the server (its uid and gid, mode
//!   0711), which binds its two sockets in it. The container's uid only
//!   needs to search it.
//! - Rootless Podman: the launcher runs as the server's user, so the chown
//!   is a no-op. When a chown is refused (`EPERM`) and the directory
//!   already belongs to the server's uid, the group is left as it is.
//! - Removal unlinks a machine directory's direct entries and removes it;
//!   it never recurses into a directory the server created.

use std::fs::{File, Metadata, Permissions};
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use agentkit_sandbox_proto::machine_dir_name;
use agentkit_sys::EntryType;

use crate::naming::WorkspaceId;
use crate::trust::{TrustRoot, refused};

/// Mode of a machine directory: traversable, not listable.
const DIR_MODE: u32 = 0o711;
/// Most entries removed from one directory.
const ENTRIES_MAX: usize = 1024;

/// The socket directory tree below the configured host directory.
#[derive(Debug, Clone)]
pub(crate) struct SocketDirs {
    host: PathBuf,
    server_uid: u32,
    server_gid: u32,
    trust: TrustRoot,
}

impl SocketDirs {
    pub(crate) fn new(host: PathBuf, server_uid: u32, server_gid: u32, trust: TrustRoot) -> Self {
        Self {
            host,
            server_uid,
            server_gid,
            trust,
        }
    }

    /// Startup check: the host directory and its ancestors pass
    /// [`Self::open_host`].
    pub(crate) fn check_host(&self) -> Result<(), String> {
        self.open_host()
            .map(drop)
            .map_err(|error| error.to_string())
    }

    /// Create, or confirm, `<host>/<machine>` of `ws` at `generation` and
    /// return its path, the bind-mount source.
    pub(crate) fn prepare(&self, ws: &WorkspaceId, generation: u64) -> io::Result<String> {
        let host = self.open_host()?;
        let name = machine_dir_name(ws.as_str(), generation);
        let dir = open_or_create(&host, &name)?;
        self.hand_to_server(&dir)?;
        dir.set_permissions(Permissions::from_mode(DIR_MODE))?;
        let path = self.host.join(name);
        path.to_str()
            .map(str::to_string)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path is not UTF-8"))
    }

    /// Remove the machine directory of `ws` at `generation` and the sockets
    /// in it. Absent is success.
    pub(crate) fn remove_generation(&self, ws: &WorkspaceId, generation: u64) -> io::Result<()> {
        let host = self.open_host()?;
        let name = machine_dir_name(ws.as_str(), generation);
        remove_flat_dir(&host, &self.host, &name)
    }

    /// The host directory: every component opened without following links,
    /// each ancestor checked, and the directory itself owned by the
    /// launcher with no permission for others and no group write; when the
    /// server is another user (not root), its group must be the server's
    /// and searchable, or the server could not reach its sockets.
    fn open_host(&self) -> io::Result<File> {
        let own = own_uid()?;
        let dir = self.trust.open(&self.host, own)?;
        self.check_host_meta(&dir.metadata()?, own)?;
        Ok(dir)
    }

    fn check_host_meta(&self, meta: &Metadata, own_uid: u32) -> io::Result<()> {
        let mode = meta.mode();
        if meta.uid() != own_uid {
            return Err(refused(&self.host, "is not owned by the launcher"));
        }
        if mode & 0o007 != 0 {
            return Err(refused(&self.host, "grants permissions to others"));
        }
        if mode & 0o020 != 0 {
            return Err(refused(&self.host, "is writable by group or others"));
        }
        let server_needs_group = self.server_uid != own_uid && self.server_uid != 0;
        if server_needs_group && (meta.gid() != self.server_gid || mode & 0o010 == 0) {
            return Err(refused(
                &self.host,
                "must be searchable by the server's group (mode 0710, group server_gid)",
            ));
        }
        Ok(())
    }

    /// Give `dir` to the server's uid and gid. An unprivileged launcher
    /// (rootless Podman) that is refused keeps the directory when it
    /// already belongs to the server's uid.
    fn hand_to_server(&self, dir: &File) -> io::Result<()> {
        let meta = dir.metadata()?;
        if meta.uid() == self.server_uid && meta.gid() == self.server_gid {
            return Ok(());
        }
        match std::os::unix::fs::fchown(dir, Some(self.server_uid), Some(self.server_gid)) {
            Err(error)
                if error.kind() == io::ErrorKind::PermissionDenied
                    && meta.uid() == self.server_uid =>
            {
                Ok(())
            }
            other => other,
        }
    }
}

/// Open the directory `name` below `parent`, creating it first when
/// absent. A link or a non-directory at `name` is refused.
fn open_or_create(parent: &File, name: &str) -> io::Result<File> {
    match agentkit_sys::create_dir_at(parent, name, DIR_MODE) {
        Err(error) if error.kind() != io::ErrorKind::AlreadyExists => return Err(error),
        _ => {}
    }
    agentkit_sys::open_dir_at_nofollow(parent, name)
}

/// Unlink every non-directory entry of `<parent>/<name>`, then remove it.
/// A directory inside it is left alone, so the removal then fails.
fn remove_flat_dir(parent: &File, parent_path: &Path, name: &str) -> io::Result<()> {
    let dir = match agentkit_sys::open_dir_at_nofollow(parent, name) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        other => other?,
    };
    for entry in entry_names(&dir)? {
        if agentkit_sys::stat_at_nofollow(&dir, &entry)?.kind != EntryType::Directory {
            agentkit_sys::unlink_at(&dir, &entry)?;
        }
    }
    // `parent` belongs to the launcher, so the path cannot be redirected;
    // `rmdir` never follows its last component.
    std::fs::remove_dir(parent_path.join(name))
}

fn entry_names(dir: &File) -> io::Result<Vec<String>> {
    let listing = agentkit_sys::read_dir_names(dir, ENTRIES_MAX)?;
    listing
        .names
        .into_iter()
        .map(|name| name.into_string().map_err(|_| not_utf8()))
        .collect()
}

fn own_uid() -> io::Result<u32> {
    agentkit_sys::effective_uid().map_err(|error| io::Error::other(error.to_string()))
}

fn not_utf8() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "name is not UTF-8")
}

#[cfg(test)]
mod tests;
