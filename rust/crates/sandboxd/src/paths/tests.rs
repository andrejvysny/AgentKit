use std::os::unix::fs::symlink;

use super::*;

#[test]
fn components_accept_only_normalised_relative_paths() {
    assert!(components("").unwrap().is_empty());
    assert_eq!(components("a").unwrap(), ["a"]);
    assert_eq!(components("a/b.c/d").unwrap(), ["a", "b.c", "d"]);
    let long_name = "n".repeat(256);
    let long_path = format!("{}a", "a/".repeat(2048));
    for bad in [
        "/abs",
        "a//b",
        "a/",
        "./a",
        "a/.",
        "..",
        "a/../b",
        "nul\0",
        long_name.as_str(),
        long_path.as_str(),
    ] {
        assert!(
            matches!(components(bad), Err(FsError::Invalid)),
            "{bad:?} must be refused"
        );
    }
}

#[test]
fn walk_refuses_a_link_anywhere_on_the_way() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("real/sub")).unwrap();
    std::fs::create_dir(outside.path().join("sub")).unwrap();
    symlink(outside.path(), root.path().join("escape")).unwrap();
    symlink(outside.path(), root.path().join("real/inner")).unwrap();
    let open = || open_root(root.path()).unwrap();
    assert!(walk(open(), &["real", "sub"], false).is_ok());
    for names in [
        &["escape"][..],
        &["escape", "sub"],
        &["real", "inner", "sub"],
    ] {
        let refused = walk(open(), names, false);
        assert!(matches!(refused, Err(FsError::Link)), "{names:?}");
        let refused = walk(open(), names, true);
        assert!(
            matches!(refused, Err(FsError::Link)),
            "{names:?} with create"
        );
    }
    assert_eq!(
        std::fs::read_dir(outside.path()).unwrap().count(),
        1,
        "nothing was created outside the root"
    );
}

#[test]
fn walk_creates_missing_directories_only_when_asked() {
    let root = tempfile::tempdir().unwrap();
    let open = || open_root(root.path()).unwrap();
    assert!(matches!(
        walk(open(), &["a", "b"], false),
        Err(FsError::NotFound)
    ));
    assert!(!root.path().join("a").exists());
    walk(open(), &["a", "b"], true).unwrap();
    assert!(root.path().join("a/b").is_dir());
    walk(open(), &["a", "b"], true).unwrap();
}

#[test]
fn walk_through_a_file_is_not_a_directory() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("file"), b"x").unwrap();
    let open = || open_root(root.path()).unwrap();
    for create in [false, true] {
        assert!(matches!(
            walk(open(), &["file", "below"], create),
            Err(FsError::NotADirectory)
        ));
    }
}

#[test]
fn error_replies_name_the_class_not_the_path() {
    let reply = FsError::Link.reply();
    let text = format!("{reply:?}");
    assert!(text.contains("InvalidPath"), "{text}");
    assert!(matches!(
        FsError::TooLarge { size: 9 }.code(),
        GuestErrorCode::FileTooLarge { size: 9 }
    ));
    assert!(matches!(
        FsError::from_io(io::Error::from(io::ErrorKind::NotFound)),
        FsError::NotFound
    ));
}
