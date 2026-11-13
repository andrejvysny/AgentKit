//! Private-file primitives: directories at mode 0700 and files at 0600.
//!
//! These are the only places the engine opens state directories with
//! secret material (per-run MCP config with the attempt-scoped broker
//! token, staged prompts). Unix applies the mode at creation *and* tightens
//! the leaf directory afterwards, because `DirBuilder::recursive` inherits
//! the umask for parents. Other platforms refuse explicitly: they have no
//! POSIX mode bits to promise.

use std::fs::File;
use std::io;
use std::path::Path;

/// Create `path` (recursive) as a 0700 directory, idempotently, tightening
/// the leaf mode even when the directory already existed.
#[cfg(unix)]
pub fn create_private_dir(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(|e| io::Error::other(format!("cannot create {}: {e}", path.display())))?;
    // recursive(true) can inherit umask for parents; tighten the leaf.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| io::Error::other(format!("cannot tighten {}: {e}", path.display())))
}

/// See [`create_private_dir`]. Platforms without POSIX mode bits refuse.
#[cfg(not(unix))]
pub fn create_private_dir(_path: &Path) -> io::Result<()> {
    Err(unsupported())
}

/// Create (or truncate) `path` as a 0600 private file, opened for writing.
#[cfg(unix)]
pub fn create_private_file(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
}

/// See [`create_private_file`]. Platforms without POSIX mode bits refuse.
#[cfg(not(unix))]
pub fn create_private_file(_path: &Path) -> io::Result<File> {
    Err(unsupported())
}

#[cfg(not(unix))]
fn unsupported() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "private file modes require POSIX mode bits",
    )
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn private_dir_is_0700_and_idempotent() {
        let root = tempfile::tempdir().unwrap();
        let nested = root.path().join("runs/attempt-1");
        create_private_dir(&nested).unwrap();
        assert_eq!(mode(&nested), 0o700);
        // Re-staging the same run dir stays 0700, even if it existed loose.
        std::fs::set_permissions(&nested, std::fs::Permissions::from_mode(0o755)).unwrap();
        create_private_dir(&nested).unwrap();
        assert_eq!(mode(&nested), 0o700);
    }

    #[test]
    fn private_file_is_0600_and_writable() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("mcp.json");
        let mut file = create_private_file(&path).unwrap();
        file.write_all(b"{}").unwrap();
        drop(file);
        assert_eq!(mode(&path), 0o600);
        assert_eq!(std::fs::read(&path).unwrap(), b"{}");
    }
}
