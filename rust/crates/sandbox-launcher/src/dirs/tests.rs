use std::os::unix::fs::symlink;

use super::*;
use crate::fake::scratch;

fn dirs(root: &Path) -> SocketDirs {
    let uid = agentkit_sys::effective_uid().unwrap();
    let gid = std::fs::metadata(root).unwrap().gid();
    SocketDirs::new(root.join("sockets"), uid, gid, TrustRoot::at(root))
}

fn ws(id: &str) -> WorkspaceId {
    WorkspaceId::parse(id).unwrap()
}

fn mode(path: &Path) -> u32 {
    std::fs::symlink_metadata(path).unwrap().mode() & 0o7777
}

fn machine(root: &Path, ws: &str, generation: u64) -> PathBuf {
    root.join("sockets").join(machine_dir_name(ws, generation))
}

#[test]
fn prepare_creates_a_flat_machine_directory_at_0711_and_is_idempotent() {
    let (_tmp, root) = scratch();
    let dirs = dirs(&root);
    dirs.check_host().unwrap();
    let path = dirs.prepare(&ws("ws-1"), 3).unwrap();
    let expected = machine(&root, "ws-1", 3);
    assert_eq!(Path::new(&path), expected);
    assert_eq!(expected.parent().unwrap(), root.join("sockets"), "flat");
    assert_eq!(mode(&expected), 0o711);
    std::fs::write(expected.join("control.sock"), b"").unwrap();
    assert_eq!(dirs.prepare(&ws("ws-1"), 3).unwrap(), path);
    assert!(expected.join("control.sock").exists(), "kept on repeat");
    assert_ne!(dirs.prepare(&ws("ws-1"), 4).unwrap(), path);
}

#[test]
fn a_link_anywhere_on_the_path_is_refused() {
    let (_tmp, root) = scratch();
    let dirs = dirs(&root);
    let outside = root.join("outside");
    std::fs::create_dir(&outside).unwrap();
    symlink(&outside, machine(&root, "ws-link", 1)).unwrap();
    assert!(dirs.prepare(&ws("ws-link"), 1).is_err());
    assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);

    symlink(root.join("sockets"), root.join("alias")).unwrap();
    let uid = agentkit_sys::effective_uid().unwrap();
    let through_link = SocketDirs::new(root.join("alias"), uid, 0, TrustRoot::at(&root));
    assert!(through_link.check_host().is_err());
    assert!(through_link.prepare(&ws("ws-3"), 1).is_err());
    assert!(!machine(&root, "ws-3", 1).exists());
}

#[test]
fn a_host_directory_open_to_others_is_refused() {
    let (_tmp, root) = scratch();
    for (host_mode, why) in [
        (0o777, "others"),
        (0o705, "others"),
        (0o701, "others"),
        (0o770, "writable"),
    ] {
        std::fs::set_permissions(root.join("sockets"), Permissions::from_mode(host_mode)).unwrap();
        let error = dirs(&root).check_host().unwrap_err();
        assert!(error.contains(why), "{host_mode:o}: {error}");
        let error = dirs(&root).prepare(&ws("ws-1"), 1).unwrap_err();
        assert!(error.to_string().contains(why), "{host_mode:o}: {error}");
    }
    for host_mode in [0o700, 0o710, 0o750] {
        std::fs::set_permissions(root.join("sockets"), Permissions::from_mode(host_mode)).unwrap();
        dirs(&root).check_host().unwrap();
    }
}

#[test]
fn another_server_user_needs_the_group_to_search_the_host_directory() {
    let (_tmp, root) = scratch();
    let uid = agentkit_sys::effective_uid().unwrap();
    if uid == u32::MAX {
        return;
    }
    let gid = std::fs::metadata(root.join("sockets")).unwrap().gid();
    let server = |gid| SocketDirs::new(root.join("sockets"), uid + 1, gid, TrustRoot::at(&root));
    std::fs::set_permissions(root.join("sockets"), Permissions::from_mode(0o700)).unwrap();
    let error = server(gid).check_host().unwrap_err();
    assert!(error.contains("server's group"), "{error}");
    std::fs::set_permissions(root.join("sockets"), Permissions::from_mode(0o710)).unwrap();
    server(gid).check_host().unwrap();
    let error = server(gid.wrapping_add(1)).check_host().unwrap_err();
    assert!(error.contains("server's group"), "{error}");
}

/// Every ancestor of the host directory is checked at each `prepare`, not
/// only at startup.
#[test]
fn an_ancestor_that_turns_writable_refuses_the_next_prepare() {
    let (_tmp, root) = scratch();
    let nested = root.join("srv");
    std::fs::create_dir(&nested).unwrap();
    std::fs::set_permissions(&nested, Permissions::from_mode(0o755)).unwrap();
    std::fs::create_dir(nested.join("sockets")).unwrap();
    std::fs::set_permissions(nested.join("sockets"), Permissions::from_mode(0o700)).unwrap();
    let uid = agentkit_sys::effective_uid().unwrap();
    let gid = std::fs::metadata(&root).unwrap().gid();
    let dirs = SocketDirs::new(nested.join("sockets"), uid, gid, TrustRoot::at(&root));
    dirs.check_host().unwrap();
    dirs.prepare(&ws("ws-1"), 1).unwrap();
    for bad in [0o775, 0o1777] {
        std::fs::set_permissions(&nested, Permissions::from_mode(bad)).unwrap();
        let error = dirs.prepare(&ws("ws-1"), 2).unwrap_err();
        assert!(error.to_string().contains("writable"), "{bad:o}: {error}");
        assert!(dirs.check_host().is_err());
    }
    let ws_2 = nested.join("sockets").join(machine_dir_name("ws-1", 2));
    assert!(!ws_2.exists(), "nothing was created");
}

#[test]
fn removal_unlinks_entries_without_following_them() {
    let (_tmp, root) = scratch();
    let dirs = dirs(&root);
    dirs.prepare(&ws("ws-1"), 1).unwrap();
    dirs.prepare(&ws("ws-1"), 2).unwrap();
    let keep = root.join("keep.txt");
    std::fs::write(&keep, b"keep").unwrap();
    let g1 = machine(&root, "ws-1", 1);
    std::fs::write(g1.join("control.sock"), b"").unwrap();
    symlink(&keep, g1.join("egress.sock")).unwrap();
    dirs.remove_generation(&ws("ws-1"), 1).unwrap();
    assert!(!g1.exists());
    assert!(machine(&root, "ws-1", 2).exists());
    assert_eq!(std::fs::read(&keep).unwrap(), b"keep");
    dirs.remove_generation(&ws("ws-1"), 1).unwrap();
    dirs.remove_generation(&ws("never"), 1).unwrap();
}

#[test]
fn removal_never_recurses_into_a_directory() {
    let (_tmp, root) = scratch();
    let dirs = dirs(&root);
    dirs.prepare(&ws("ws-1"), 1).unwrap();
    let nested = machine(&root, "ws-1", 1).join("nested");
    std::fs::create_dir(&nested).unwrap();
    std::fs::write(nested.join("file"), b"x").unwrap();
    assert!(dirs.remove_generation(&ws("ws-1"), 1).is_err());
    assert!(nested.join("file").exists());
}
