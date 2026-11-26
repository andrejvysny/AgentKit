use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::PermissionsExt;

use super::*;
use crate::fs::open_dir_nofollow;

fn root() -> (tempfile::TempDir, File) {
    let root = tempfile::tempdir().unwrap();
    let dir = open_dir_nofollow(root.path()).unwrap();
    (root, dir)
}

#[test]
fn names_that_are_not_one_component_are_refused() {
    let (_root, dir) = root();
    for name in ["", ".", "..", "a/b", "/abs", "nul\0"] {
        let error = open_dir_at_nofollow(&dir, name).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{name:?}");
        let error = create_dir_at(&dir, name, 0o755).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{name:?}");
        let error = stat_at_nofollow(&dir, name).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{name:?}");
    }
}

#[test]
fn open_dir_at_steps_into_a_directory_and_refuses_a_link() {
    let (root, dir) = root();
    create_dir_at(&dir, "sub", 0o755).unwrap();
    let error = create_dir_at(&dir, "sub", 0o755).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    let sub = open_dir_at_nofollow(&dir, "sub").unwrap();
    assert!(sub.metadata().unwrap().is_dir());
    std::os::unix::fs::symlink(root.path().join("sub"), root.path().join("link")).unwrap();
    let error = open_dir_at_nofollow(&dir, "link").unwrap_err();
    assert!(
        matches!(
            error.raw_os_error(),
            Some(libc::ELOOP) | Some(libc::ENOTDIR)
        ),
        "a linked directory must be refused: {error}"
    );
    std::fs::write(root.path().join("file"), b"x").unwrap();
    let error = open_dir_at_nofollow(&dir, "file").unwrap_err();
    assert_eq!(error.raw_os_error(), Some(libc::ENOTDIR));
    let error = open_dir_at_nofollow(&dir, "missing").unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::NotFound);
}

#[test]
fn stat_at_reports_kinds_without_following() {
    let (root, dir) = root();
    std::fs::write(root.path().join("file"), b"hello").unwrap();
    std::fs::create_dir(root.path().join("dir")).unwrap();
    std::os::unix::fs::symlink("file", root.path().join("link")).unwrap();
    let file = stat_at_nofollow(&dir, "file").unwrap();
    assert_eq!((file.kind, file.size), (EntryType::File, 5));
    assert!(file.mtime_ms > 0);
    assert_eq!(
        stat_at_nofollow(&dir, "dir").unwrap().kind,
        EntryType::Directory
    );
    assert_eq!(
        stat_at_nofollow(&dir, "link").unwrap().kind,
        EntryType::Symlink
    );
    let error = stat_at_nofollow(&dir, "missing").unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::NotFound);
}

#[test]
fn stat_at_reports_allocated_space_apart_from_size() {
    use std::os::unix::fs::MetadataExt;
    let (root, dir) = root();
    let sparse = root.path().join("sparse");
    File::create(&sparse).unwrap().set_len(1 << 30).unwrap();
    let stat = stat_at_nofollow(&dir, "sparse").unwrap();
    let meta = std::fs::metadata(&sparse).unwrap();
    assert_eq!(stat.size, 1 << 30);
    assert_eq!(stat.allocated, meta.blocks() * 512);
    assert!(stat.allocated < stat.size, "a hole takes no space");
}

#[test]
fn read_dir_names_lists_bounded_and_leaves_the_anchor_usable() {
    let (root, dir) = root();
    for name in ["a", "b", "c"] {
        std::fs::write(root.path().join(name), b"").unwrap();
    }
    let mut all = read_dir_names(&dir, 10).unwrap();
    all.names.sort();
    assert_eq!(all.names, ["a", "b", "c"]);
    assert!(!all.truncated);
    let capped = read_dir_names(&dir, 2).unwrap();
    assert_eq!((capped.names.len(), capped.truncated), (2, true));
    let again = read_dir_names(&dir, 10).unwrap();
    assert_eq!(again.names.len(), 3, "a second read starts over");
    assert!(
        stat_at_nofollow(&dir, "a").is_ok(),
        "the anchor still works"
    );
}

#[test]
fn create_new_rw_file_at_is_exclusive_readable_and_refuses_links() {
    let (root, dir) = root();
    let mut file = create_new_rw_file_at(&dir, "log", 0o640).unwrap();
    file.write_all(b"line\n").unwrap();
    file.seek(SeekFrom::Start(0)).unwrap();
    let mut text = String::new();
    file.read_to_string(&mut text).unwrap();
    assert_eq!(text, "line\n");
    let mode = file.metadata().unwrap().permissions().mode() & 0o777;
    assert_eq!(mode & !0o640, 0, "mode {mode:o} within 0640");
    let error = create_new_rw_file_at(&dir, "log", 0o640).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    let outside = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path().join("x"), root.path().join("planted")).unwrap();
    assert!(create_new_rw_file_at(&dir, "planted", 0o640).is_err());
    assert!(
        !outside.path().join("x").exists(),
        "nothing created outside"
    );
    sync_dir(&dir).unwrap();
}
