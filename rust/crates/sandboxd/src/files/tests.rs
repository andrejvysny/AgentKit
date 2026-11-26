use std::os::unix::fs::symlink;

use agentkit_sandbox_proto::guest::PathRoot;

use super::*;

fn req(path: &str) -> ReadRequest {
    ReadRequest {
        root: PathRoot::Workspace,
        path: path.to_owned(),
        offset: 0,
        limit_bytes: 10 << 20,
        max_file_bytes: 10 << 20,
        raw: false,
    }
}

#[test]
fn text_is_read_whole_or_by_window() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("dir")).unwrap();
    std::fs::write(root.path().join("dir/a.txt"), b"hello world").unwrap();
    let (header, bytes) = read_file(root.path(), &req("dir/a.txt")).unwrap();
    assert_eq!(bytes, b"hello world");
    assert_eq!((header.size, header.binary, header.eof), (11, false, true));
    assert_eq!(header.len, 11);
    assert!(header.mtime_ms > 0);
    let part = ReadRequest {
        offset: 6,
        limit_bytes: 3,
        ..req("dir/a.txt")
    };
    let (header, bytes) = read_file(root.path(), &part).unwrap();
    assert_eq!(
        (bytes.as_slice(), header.size, header.eof),
        (&b"wor"[..], 11, false)
    );
    let past = ReadRequest {
        offset: 50,
        ..req("dir/a.txt")
    };
    let (header, bytes) = read_file(root.path(), &past).unwrap();
    assert!(bytes.is_empty() && header.eof);
}

#[test]
fn binary_has_no_bytes_unless_raw() {
    let root = tempfile::tempdir().unwrap();
    let content = [0_u8, 1, 2, 0xff, b'\n'];
    std::fs::write(root.path().join("image.bin"), content).unwrap();
    let (header, bytes) = read_file(root.path(), &req("image.bin")).unwrap();
    assert!(header.binary && bytes.is_empty() && header.len == 0);
    let raw = ReadRequest {
        raw: true,
        ..req("image.bin")
    };
    let (header, bytes) = read_file(root.path(), &raw).unwrap();
    assert_eq!(
        (header.binary, bytes.as_slice(), header.eof),
        (true, &content[..], true)
    );
    std::fs::write(root.path().join("latin1.txt"), b"caf\xe9").unwrap();
    assert!(read_file(root.path(), &req("latin1.txt")).unwrap().0.binary);
}

#[test]
fn a_nul_or_bad_utf8_late_in_the_file_makes_every_window_binary() {
    let root = tempfile::tempdir().unwrap();
    let mut content = vec![b'a'; 200_000];
    content.push(0);
    std::fs::write(root.path().join("late.bin"), &content).unwrap();
    let head = ReadRequest {
        limit_bytes: 10,
        ..req("late.bin")
    };
    let (header, bytes) = read_file(root.path(), &head).unwrap();
    assert!(header.binary && bytes.is_empty());
}

#[test]
fn utf8_split_across_scan_chunks_is_text() {
    let mut check = TextCheck::default();
    let euro = "€".as_bytes();
    check.feed(&euro[..1]);
    check.feed(&euro[1..]);
    assert!(!check.finish());
    let mut cut = TextCheck::default();
    cut.feed(&euro[..2]);
    assert!(
        cut.finish(),
        "a sequence cut at the end of the file is binary"
    );
    let root = tempfile::tempdir().unwrap();
    let mut content = vec![b'x'; SCAN_CHUNK - 1];
    content.extend_from_slice(euro);
    std::fs::write(root.path().join("edge.txt"), &content).unwrap();
    let (header, bytes) = read_file(root.path(), &req("edge.txt")).unwrap();
    assert!(!header.binary);
    assert_eq!(bytes, content);
}

#[test]
fn missing_directories_and_special_targets_are_refused() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("dir")).unwrap();
    std::fs::write(root.path().join("dir/a.txt"), b"hello world").unwrap();
    symlink(root.path().join("dir/a.txt"), root.path().join("link")).unwrap();
    let outcome = |path: &str| read_file(root.path(), &req(path)).map(|_| ());
    assert!(matches!(outcome("missing.txt"), Err(FsError::NotFound)));
    assert!(matches!(outcome("absent/a.txt"), Err(FsError::NotFound)));
    assert!(matches!(outcome("dir"), Err(FsError::NotARegularFile)));
    assert!(matches!(outcome(""), Err(FsError::NotARegularFile)));
    assert!(matches!(outcome("link"), Err(FsError::NotARegularFile)));
    assert!(matches!(
        outcome("dir/a.txt/x"),
        Err(FsError::NotADirectory)
    ));
    assert!(matches!(outcome("../x"), Err(FsError::Invalid)));
}

#[test]
fn a_linked_directory_on_the_way_is_refused() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret"), b"outside").unwrap();
    symlink(outside.path(), root.path().join("escape")).unwrap();
    let refused = read_file(root.path(), &req("escape/secret"));
    assert!(matches!(refused, Err(FsError::Link)));
}

#[test]
fn files_over_the_cap_are_too_large() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.txt"), b"hello world").unwrap();
    let capped = ReadRequest {
        max_file_bytes: 4,
        ..req("a.txt")
    };
    let refused = read_file(root.path(), &capped);
    assert!(matches!(refused, Err(FsError::TooLarge { size: 11 })));
    let exact = ReadRequest {
        max_file_bytes: 11,
        ..req("a.txt")
    };
    assert!(read_file(root.path(), &exact).is_ok());
}
