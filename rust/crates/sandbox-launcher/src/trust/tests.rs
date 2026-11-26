use std::fs::Permissions;
use std::os::unix::fs::{PermissionsExt, symlink};

use super::*;

fn uid() -> u32 {
    agentkit_sys::effective_uid().unwrap()
}

/// A canonical scratch root with `a/b/c`, each 0755, the root too (it is
/// checked, and a umask of 002 would leave it group-writable).
fn tree() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    std::fs::create_dir_all(root.join("a/b/c")).unwrap();
    for sub in ["", "a", "a/b", "a/b/c"] {
        std::fs::set_permissions(root.join(sub), Permissions::from_mode(0o755)).unwrap();
    }
    (dir, root)
}

#[test]
fn the_system_root_checks_every_directory_from_slash() {
    // `/` and `/usr` belong to root and are not writable by others on
    // every supported host.
    TrustRoot::system().open(Path::new("/usr"), uid()).unwrap();
    TrustRoot::system()
        .open_checked(Path::new("/usr"), uid())
        .unwrap();
}

#[test]
fn a_plain_tree_owned_by_the_launcher_is_accepted() {
    let (_tmp, root) = tree();
    let trust = TrustRoot::at(&root);
    trust.open_checked(&root.join("a/b/c"), uid()).unwrap();
}

#[test]
fn an_ancestor_writable_by_group_or_others_is_refused() {
    let (_tmp, root) = tree();
    let trust = TrustRoot::at(&root);
    for mode in [0o775, 0o757, 0o1777] {
        std::fs::set_permissions(root.join("a"), Permissions::from_mode(mode)).unwrap();
        let error = trust.open(&root.join("a/b/c"), uid()).unwrap_err();
        assert!(error.to_string().contains("writable"), "{mode:o}: {error}");
    }
    std::fs::set_permissions(root.join("a"), Permissions::from_mode(0o755)).unwrap();
    std::fs::set_permissions(root.join("a/b/c"), Permissions::from_mode(0o777)).unwrap();
    trust.open(&root.join("a/b/c"), uid()).unwrap();
    let error = trust.open_checked(&root.join("a/b/c"), uid()).unwrap_err();
    assert!(error.to_string().contains("writable"), "{error}");
}

#[test]
fn an_ancestor_of_another_owner_is_refused() {
    if uid() == 0 {
        return; // root's tree is trusted by every launcher.
    }
    let (_tmp, root) = tree();
    let trust = TrustRoot::at(&root);
    // The tree belongs to the test user; a launcher of another uid that is
    // not root must refuse it.
    let other = uid() + 1;
    let error = trust.open(&root.join("a/b/c"), other).unwrap_err();
    assert!(error.to_string().contains("not owned"), "{error}");
}

#[test]
fn a_link_anywhere_on_the_path_is_refused() {
    let (_tmp, root) = tree();
    let trust = TrustRoot::at(&root);
    symlink(root.join("a/b"), root.join("link")).unwrap();
    assert!(trust.open(&root.join("link/c"), uid()).is_err());
    assert!(trust.open(&root.join("link"), uid()).is_err());
}

#[test]
fn a_path_outside_the_trusted_root_is_refused() {
    let (_tmp, root) = tree();
    let trust = TrustRoot::at(&root.join("a/b"));
    let error = trust.open(&root.join("a"), uid()).unwrap_err();
    assert!(error.to_string().contains("trusted root"), "{error}");
}

#[test]
fn a_private_parent_must_belong_to_the_launcher() {
    let (_tmp, root) = tree();
    let dir = root.join("a");
    let meta = std::fs::metadata(&dir).unwrap();
    check_private_parent(&meta, uid(), &dir).unwrap();
    let error = check_private_parent(&meta, uid().wrapping_add(1), &dir).unwrap_err();
    assert!(error.to_string().contains("not owned"), "{error}");
    std::fs::set_permissions(&dir, Permissions::from_mode(0o775)).unwrap();
    let meta = std::fs::metadata(&dir).unwrap();
    let error = check_private_parent(&meta, uid(), &dir).unwrap_err();
    assert!(error.to_string().contains("writable"), "{error}");
}
