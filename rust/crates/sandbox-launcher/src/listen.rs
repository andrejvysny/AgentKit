//! The launcher's own listen socket. Its mode and group are set by path
//! after `bind` (and `chmod` follows links), so the socket's directory
//! must leave nobody else a way to replace the socket in between: the
//! directory belongs to the launcher and is not writable by group or
//! others, and every directory above it belongs to root or the launcher
//! and is not writable by group or others ([`crate::trust`]).

use std::fs::Permissions;
use std::io;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::Path;

use tokio::net::UnixListener;

use crate::config::Config;
use crate::trust::{TrustRoot, check_private_parent, refused};

/// Check the socket's directory, replace a stale socket file (never
/// anything else), bind, then set the mode and group.
pub(crate) fn bind_listener(
    config: &Config,
    trust: &TrustRoot,
    own_uid: u32,
) -> io::Result<UnixListener> {
    let path = &config.listen_socket;
    let parent = path
        .parent()
        .filter(|parent| *parent != Path::new(""))
        .ok_or_else(|| refused(path, "has no parent directory"))?;
    let dir = trust.open(parent, own_uid)?;
    check_private_parent(&dir.metadata()?, own_uid, parent)?;
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_socket() => std::fs::remove_file(path)?,
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "listen_socket exists and is not a socket",
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let listener = UnixListener::bind(path)?;
    if let Some(gid) = config.listen_gid {
        std::os::unix::fs::lchown(path, None, Some(gid))?;
    }
    std::fs::set_permissions(path, Permissions::from_mode(config.listen_mode))?;
    Ok(listener)
}

#[cfg(test)]
mod tests;
