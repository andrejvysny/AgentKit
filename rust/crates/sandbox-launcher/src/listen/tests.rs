use std::os::unix::fs::{MetadataExt, symlink};

use super::*;
use crate::fake::{scratch, test_config};

fn uid() -> u32 {
    agentkit_sys::effective_uid().unwrap()
}

fn mode(path: &Path) -> u32 {
    std::fs::symlink_metadata(path).unwrap().mode() & 0o777
}

#[tokio::test]
async fn a_private_directory_gets_the_socket_with_its_mode() {
    let (_tmp, root) = scratch();
    let config = test_config(&root);
    let trust = TrustRoot::at(&root);
    let listener = bind_listener(&config, &trust, uid()).unwrap();
    assert_eq!(mode(&config.listen_socket), 0o600);
    drop(listener);
    // A stale socket is replaced; anything else is refused.
    bind_listener(&config, &trust, uid()).unwrap();
    std::fs::remove_file(&config.listen_socket).unwrap();
    std::fs::write(&config.listen_socket, b"x").unwrap();
    assert!(bind_listener(&config, &trust, uid()).is_err());
    assert_eq!(std::fs::read(&config.listen_socket).unwrap(), b"x");
}

#[tokio::test]
async fn a_directory_others_could_change_is_refused_before_bind() {
    let (_tmp, root) = scratch();
    let dir = root.join("run");
    std::fs::create_dir(&dir).unwrap();
    let config = Config {
        listen_socket: dir.join("launcher.sock"),
        ..test_config(&root)
    };
    let trust = TrustRoot::at(&root);
    for bad in [0o775, 0o757, 0o1777] {
        std::fs::set_permissions(&dir, Permissions::from_mode(bad)).unwrap();
        let error = bind_listener(&config, &trust, uid()).unwrap_err();
        assert!(error.to_string().contains("writable"), "{bad:o}: {error}");
        assert!(!config.listen_socket.exists(), "{bad:o}: nothing bound");
    }
    std::fs::set_permissions(&dir, Permissions::from_mode(0o755)).unwrap();
    let error = bind_listener(&config, &trust, uid() + 1).unwrap_err();
    assert!(error.to_string().contains("not owned"), "{error}");
    bind_listener(&config, &trust, uid()).unwrap();
}

#[tokio::test]
async fn a_linked_directory_is_refused() {
    let (_tmp, root) = scratch();
    let real = root.join("real");
    std::fs::create_dir(&real).unwrap();
    std::fs::set_permissions(&real, Permissions::from_mode(0o755)).unwrap();
    symlink(&real, root.join("alias")).unwrap();
    let config = Config {
        listen_socket: root.join("alias/launcher.sock"),
        ..test_config(&root)
    };
    assert!(bind_listener(&config, &TrustRoot::at(&root), uid()).is_err());
    assert!(!real.join("launcher.sock").exists());
}
