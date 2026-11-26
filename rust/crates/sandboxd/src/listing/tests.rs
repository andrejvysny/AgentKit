use std::collections::BTreeSet;
use std::os::unix::fs::symlink;

use agentkit_sandbox_proto::guest::PathRoot;

use super::*;

fn req(path: &str, depth: u8, max_entries: u32) -> ListRequest {
    ListRequest {
        root: PathRoot::Workspace,
        path: path.to_owned(),
        depth,
        max_entries,
    }
}

fn entry(path: &str, kind: EntryKind, size: Option<u64>) -> Entry {
    Entry {
        path: path.to_owned(),
        kind,
        size,
    }
}

fn set(entries: Vec<Entry>) -> BTreeSet<String> {
    entries.iter().map(|entry| format!("{entry:?}")).collect()
}

fn tree() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("tree/d/e")).unwrap();
    for path in ["tree/a.txt", "tree/d/b.txt", "tree/d/e/c.txt"] {
        std::fs::write(root.path().join(path), b"abc").unwrap();
    }
    root
}

#[test]
fn depth_limits_how_far_the_listing_goes() {
    let root = tree();
    let a = entry("tree/a.txt", EntryKind::File, Some(3));
    let d = entry("tree/d", EntryKind::Directory, None);
    let b = entry("tree/d/b.txt", EntryKind::File, Some(3));
    let e = entry("tree/d/e", EntryKind::Directory, None);
    let c = entry("tree/d/e/c.txt", EntryKind::File, Some(3));
    let cases = [
        (1, vec![a.clone(), d.clone()]),
        (2, vec![a.clone(), d.clone(), b.clone(), e.clone()]),
        (4, vec![a.clone(), d.clone(), b, e, c]),
        (0, vec![a, d]),
    ];
    for (depth, expected) in cases {
        let (entries, truncated) = list(root.path(), &req("tree", depth, 500)).unwrap();
        assert!(!truncated, "depth {depth}");
        assert_eq!(set(entries), set(expected), "depth {depth}");
    }
}

#[test]
fn the_count_cap_truncates_the_same_prefix_every_time() {
    let root = tree();
    let (first, truncated) = list(root.path(), &req("tree", 4, 2)).unwrap();
    assert_eq!((first.len(), truncated), (2, true));
    let (again, _) = list(root.path(), &req("tree", 4, 2)).unwrap();
    assert_eq!(first, again);
    let (all, truncated) = list(root.path(), &req("tree", 4, 5)).unwrap();
    assert_eq!((all.len(), truncated), (5, false), "exactly the cap");
    let (none, truncated) = list(root.path(), &req("tree", 1, 0)).unwrap();
    assert!(none.is_empty() && truncated);
}

#[test]
fn links_are_listed_never_followed() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret"), b"x").unwrap();
    symlink(outside.path(), root.path().join("escape")).unwrap();
    let (entries, _) = list(root.path(), &req("", 4, 500)).unwrap();
    assert_eq!(entries, [entry("escape", EntryKind::Symlink, None)]);
    let refused = list(root.path(), &req("escape", 1, 500));
    assert!(matches!(refused, Err(FsError::Link)));
}

#[test]
fn missing_and_non_directories_are_refused() {
    let root = tree();
    assert!(matches!(
        list(root.path(), &req("absent", 1, 10)),
        Err(FsError::NotFound)
    ));
    assert!(matches!(
        list(root.path(), &req("tree/a.txt", 1, 10)),
        Err(FsError::NotADirectory)
    ));
    assert!(matches!(
        list(root.path(), &req("/tree", 1, 10)),
        Err(FsError::Invalid)
    ));
}

#[test]
fn entries_are_cut_to_fit_one_control_frame() {
    let long = "x".repeat(250);
    let mut entries: Vec<Entry> = (0..500)
        .map(|n| entry(&format!("{long}/{n:03}{long}"), EntryKind::File, Some(1)))
        .collect();
    assert!(fit_frame(&mut entries));
    assert!(entries.len() < 500 && !entries.is_empty());
    let reply = Reply::Entries {
        entries,
        truncated: true,
    };
    assert!(
        agentkit_sandbox_proto::encode_control(&agentkit_sandbox_proto::Envelope::new(
            u64::MAX,
            reply
        ))
        .is_ok()
    );
    let mut few = vec![entry("a", EntryKind::File, Some(1))];
    assert!(!fit_frame(&mut few));
    assert_eq!(few.len(), 1);
}

// APFS refuses names that are not UTF-8 (EILSEQ), so this runs on Linux.
#[cfg(target_os = "linux")]
#[test]
fn a_name_that_is_not_utf8_is_left_out_and_marks_the_listing_truncated() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("plain.txt"), b"abc").unwrap();
    std::fs::write(root.path().join(OsStr::from_bytes(b"odd-\xff")), b"x").unwrap();
    let (entries, truncated) = list(root.path(), &req("", 1, 500)).unwrap();
    assert_eq!(entries, [entry("plain.txt", EntryKind::File, Some(3))]);
    assert!(truncated, "a left-out name is reported");
}
