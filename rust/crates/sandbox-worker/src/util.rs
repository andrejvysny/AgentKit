//! Small OS-facing helpers (permissions, atomic writes).

use std::path::Path;

/// Best-effort `chmod` for directories we create under our storage root.
/// Failure is logged, never swallowed silently into success claims — the
/// caller's effects remain journaled and honest about what happened.
pub fn set_dir_mode(path: &Path, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)) {
            eprintln!("audit: chmod {:o} failed for {}: {e}", mode, path.display());
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
}
