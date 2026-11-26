use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::PathBuf;

use super::*;

/// What the walk should count for the file at `path`.
fn counted(path: &Path) -> u64 {
    let meta = std::fs::symlink_metadata(path).unwrap();
    meta.len().max(meta.blocks() * 512)
}

fn complete(bytes: u64) -> TreeBytes {
    TreeBytes {
        bytes,
        complete: true,
    }
}

/// Permission checks do not bind root, so chmod cases cannot hide
/// anything from it.
fn is_root() -> bool {
    agentkit_sys::effective_uid().unwrap() == 0
}

/// Restores a directory's mode when dropped, so the scratch tree can be
/// removed even when an assertion fails.
struct Restore(PathBuf);

impl Drop for Restore {
    fn drop(&mut self) {
        let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
    }
}

#[test]
fn counts_regular_files_below_the_root_only() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("big"), vec![0; 100_000]).unwrap();
    assert_eq!(tree_bytes(root.path()).unwrap(), complete(0));
    std::fs::create_dir_all(root.path().join("a/b")).unwrap();
    let top = root.path().join("top.txt");
    let deep = root.path().join("a/b/deep.txt");
    std::fs::write(&top, vec![1; 10]).unwrap();
    std::fs::write(&deep, vec![1; 65_536]).unwrap();
    symlink(outside.path(), root.path().join("escape")).unwrap();
    symlink(outside.path().join("big"), root.path().join("a/big")).unwrap();
    let expected = counted(&top) + counted(&deep);
    assert!(expected >= 65_546);
    assert_eq!(tree_bytes(root.path()).unwrap(), complete(expected));
}

#[test]
fn a_file_counts_its_apparent_size_or_its_allocation_whichever_is_larger() {
    let root = tempfile::tempdir().unwrap();
    let sparse = root.path().join("sparse");
    File::create(&sparse).unwrap().set_len(1 << 30).unwrap();
    let stat = std::fs::metadata(&sparse).unwrap();
    assert!(stat.blocks() * 512 < stat.len(), "the file is sparse");
    assert_eq!(tree_bytes(root.path()).unwrap(), complete(1 << 30));
    let small = root.path().join("small");
    std::fs::write(&small, b"x").unwrap();
    let measured = tree_bytes(root.path()).unwrap();
    assert_eq!(measured, complete((1 << 30) + counted(&small)));
    let rule = |size, allocated| {
        file_bytes(&EntryStat {
            kind: EntryType::File,
            size,
            allocated,
            mtime_ms: 0,
        })
    };
    assert_eq!(rule(1 << 30, 4096), 1 << 30, "a hole still counts");
    assert_eq!(rule(1, 1 << 20), 1 << 20, "reserved space counts");
}

/// `levels` nested directories below `root`; answers the deepest.
fn nest(root: &Path, levels: u32) -> PathBuf {
    let mut path = root.to_path_buf();
    for _ in 0..levels {
        path.push("d");
    }
    std::fs::create_dir_all(&path).unwrap();
    path
}

#[test]
fn nesting_past_the_depth_limit_is_incomplete() {
    let root = tempfile::tempdir().unwrap();
    let deepest = nest(root.path(), USAGE_DEPTH_MAX - 1);
    let file = deepest.join("f");
    std::fs::write(&file, vec![1; 100]).unwrap();
    let at_limit = tree_bytes(root.path()).unwrap();
    assert_eq!(at_limit, complete(counted(&file)), "the last walked level");
    std::fs::create_dir(deepest.join("d")).unwrap();
    std::fs::write(deepest.join("d/hidden"), vec![1; 100_000]).unwrap();
    let past = tree_bytes(root.path()).unwrap();
    assert_eq!(past.bytes, counted(&file), "measures what it can");
    assert!(!past.complete, "a level below the limit was not measured");
}

#[test]
fn an_unreadable_directory_is_incomplete() {
    if is_root() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let seen = root.path().join("seen.txt");
    std::fs::write(&seen, vec![1; 10]).unwrap();
    let locked = root.path().join("locked");
    std::fs::create_dir(&locked).unwrap();
    std::fs::write(locked.join("big"), vec![1; 1 << 20]).unwrap();
    let _restore = Restore(locked.clone());
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let measured = tree_bytes(root.path()).unwrap();
    assert_eq!(measured.bytes, counted(&seen), "measures what it can");
    assert!(!measured.complete, "a chmod 000 directory hides its files");
}

#[test]
fn a_directory_whose_entries_cannot_be_examined_is_incomplete() {
    if is_root() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let listed = root.path().join("listed");
    std::fs::create_dir(&listed).unwrap();
    std::fs::write(listed.join("big"), vec![1; 1 << 20]).unwrap();
    let _restore = Restore(listed.clone());
    // Readable but not searchable: the names list, their sizes do not.
    std::fs::set_permissions(&listed, std::fs::Permissions::from_mode(0o400)).unwrap();
    let measured = tree_bytes(root.path()).unwrap();
    assert_eq!(measured.bytes, 0);
    assert!(!measured.complete);
}

#[test]
fn a_missing_root_is_an_error() {
    let root = tempfile::tempdir().unwrap();
    assert!(matches!(
        tree_bytes(&root.path().join("absent")),
        Err(FsError::NotFound)
    ));
}

// APFS refuses names that are not UTF-8 (EILSEQ), so this runs on Linux.
#[cfg(target_os = "linux")]
#[test]
fn a_name_that_is_not_utf8_is_incomplete() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let root = tempfile::tempdir().unwrap();
    let odd = root.path().join(OsStr::from_bytes(b"odd-\xff"));
    std::fs::write(odd, vec![1; 1 << 20]).unwrap();
    let measured = tree_bytes(root.path()).unwrap();
    assert_eq!(measured.bytes, 0);
    assert!(!measured.complete);
}
