use std::fs::Permissions;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::PathBuf;

use super::*;

const BYTES_4: OutputLimit = OutputLimit {
    max_bytes: 4,
    max_lines: None,
};

#[test]
fn admit_keeps_up_to_the_byte_limit() {
    assert_eq!(admit(0, 0, b"abc", &BYTES_4), (3, 0));
    assert_eq!(admit(3, 0, b"de", &BYTES_4), (1, 0));
    assert_eq!(admit(4, 0, b"f", &BYTES_4), (0, 0));
    assert_eq!(admit(0, 0, b"abcd", &BYTES_4), (4, 0), "exactly the limit");
}

#[test]
fn admit_stops_after_the_last_allowed_line() {
    let two_lines = OutputLimit {
        max_bytes: 100,
        max_lines: Some(2),
    };
    assert_eq!(admit(0, 0, b"a\nb\nc", &two_lines), (4, 2));
    assert_eq!(
        admit(0, 0, b"a\nb\n", &two_lines),
        (4, 2),
        "exactly two lines"
    );
    assert_eq!(admit(2, 1, b"b\nc\n", &two_lines), (2, 2));
    assert_eq!(admit(4, 2, b"c", &two_lines), (0, 2));
    assert_eq!(admit(0, 0, b"no newline", &two_lines), (10, 0));
    let tight = OutputLimit {
        max_bytes: 3,
        max_lines: Some(5),
    };
    assert_eq!(admit(0, 0, b"a\nb\nc\n", &tight), (3, 1), "bytes first");
}

fn log_in(root: &Path, op: &str, limit: OutputLimit) -> OpLog {
    OpLog::create(root, op, limit).unwrap()
}

#[test]
fn log_holds_at_most_the_limit_and_reports_the_drop() {
    let root = tempfile::tempdir().unwrap();
    let log = log_in(root.path(), "op-1", BYTES_4);
    assert!(log.append(b"ab"));
    assert!(!log.append(b"cdef"));
    assert!(log.over_limit() && log.dropped());
    assert_eq!(log.written(), 4);
    assert_eq!(log.read(root.path(), 0, 100), b"abcd");
    assert_eq!(log.read(root.path(), 2, 1), b"c");
    let on_disk = std::fs::read(root.path().join(".sandbox/logs/op-1.log")).unwrap();
    assert_eq!(on_disk, b"abcd");
}

#[test]
fn a_closed_log_reopens_only_the_same_file() {
    let root = tempfile::tempdir().unwrap();
    let log = log_in(root.path(), "op-1", BYTES_4);
    log.append(b"hi");
    log.close();
    assert_eq!(log.read(root.path(), 0, 2), b"hi", "reopened by name");
    let path = root.path().join(".sandbox/logs/op-1.log");
    // Both inodes exist at once, so the replacement cannot reuse the number.
    let forged = root.path().join(".sandbox/logs/forged");
    std::fs::write(&forged, b"forged").unwrap();
    std::fs::rename(&forged, &path).unwrap();
    assert!(log.read(root.path(), 0, 2).is_empty(), "another inode");
    std::fs::remove_file(&path).unwrap();
    symlink(root.path().join("elsewhere"), &path).unwrap();
    assert!(log.read(root.path(), 0, 2).is_empty(), "a link is refused");
}

#[test]
fn an_existing_log_name_is_replaced_not_followed() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let logs = root.path().join(".sandbox/logs");
    std::fs::create_dir_all(&logs).unwrap();
    std::fs::write(logs.join("old.log"), b"previous generation").unwrap();
    let log = log_in(root.path(), "old", BYTES_4);
    log.append(b"new");
    assert_eq!(std::fs::read(logs.join("old.log")).unwrap(), b"new");
    symlink(outside.path().join("target"), logs.join("planted.log")).unwrap();
    let planted = log_in(root.path(), "planted", BYTES_4);
    planted.append(b"x");
    assert!(
        !outside.path().join("target").exists(),
        "nothing written outside"
    );
}

/// The entries of `root` that a repair set aside.
fn set_aside_entries(root: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(SET_ASIDE_PREFIX))
        })
        .collect()
}

/// Gives every directory a test locked back its mode when dropped, so the
/// scratch tree can be removed whatever the test did.
struct Unlock(PathBuf);

impl Drop for Unlock {
    fn drop(&mut self) {
        let open = |path: &Path| {
            if std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir()) {
                let _ = std::fs::set_permissions(path, Permissions::from_mode(0o755));
            }
        };
        open(&self.0);
        for dir in [self.0.join(".sandbox"), self.0.join(".sandbox/logs")] {
            open(&dir);
        }
        for aside in set_aside_entries(&self.0) {
            open(&aside);
            open(&aside.join("logs"));
        }
    }
}

fn mode_of(path: &Path) -> u32 {
    std::fs::symlink_metadata(path)
        .unwrap()
        .permissions()
        .mode()
        & 0o7777
}

/// A log can be made and read back, the log directories are fresh 0700
/// directories, and exactly `aside` entries were set aside.
fn assert_repaired(root: &Path, op: &str, aside: usize) {
    let log = log_in(root, op, BYTES_4);
    assert!(log.append(b"ok"));
    assert_eq!(log.read(root, 0, 2), b"ok");
    let sandbox = root.join(".sandbox");
    for dir in [sandbox.clone(), sandbox.join("logs")] {
        let meta = std::fs::symlink_metadata(&dir).unwrap();
        assert!(meta.is_dir(), "{dir:?} is a real directory");
        assert_eq!(mode_of(&dir), 0o700, "{dir:?}");
    }
    let file = std::fs::symlink_metadata(sandbox.join("logs").join(format!("{op}.log")));
    assert!(file.unwrap().is_file());
    assert_eq!(set_aside_entries(root).len(), aside);
}

#[test]
fn a_linked_log_directory_is_set_aside_never_followed() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    symlink(outside.path(), root.path().join(".sandbox")).unwrap();
    assert_repaired(root.path(), "op-1", 1);
    let aside = &set_aside_entries(root.path())[0];
    assert_eq!(
        std::fs::read_link(aside).unwrap(),
        outside.path(),
        "the link itself moved"
    );
    std::fs::remove_dir_all(root.path().join(".sandbox/logs")).unwrap();
    symlink(outside.path(), root.path().join(".sandbox/logs")).unwrap();
    assert_repaired(root.path(), "op-2", 2);
    assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
}

#[test]
fn a_log_directory_that_is_not_a_directory_is_set_aside() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join(".sandbox"), b"not a directory").unwrap();
    assert_repaired(root.path(), "op-1", 1);
    std::fs::remove_dir_all(root.path().join(".sandbox/logs")).unwrap();
    std::fs::write(root.path().join(".sandbox/logs"), b"").unwrap();
    assert_repaired(root.path(), "op-2", 2);
    // A directory where the log file goes cannot be unlinked as a file.
    std::fs::create_dir(root.path().join(".sandbox/logs/op-3.log")).unwrap();
    assert_repaired(root.path(), "op-3", 3);
}

#[test]
fn a_log_directory_locked_by_the_workload_is_set_aside() {
    let root = tempfile::tempdir().unwrap();
    let _unlock = Unlock(root.path().to_path_buf());
    let logs = root.path().join(".sandbox/logs");
    log_in(root.path(), "op-0", BYTES_4);
    for (n, (dir, mode)) in [
        (logs.clone(), 0o000),
        (logs.clone(), 0o500),
        (root.path().join(".sandbox"), 0o000),
        (root.path().join(".sandbox"), 0o600),
    ]
    .into_iter()
    .enumerate()
    {
        std::fs::set_permissions(&dir, Permissions::from_mode(mode)).unwrap();
        assert_repaired(root.path(), &format!("op-{}", n + 1), n + 1);
    }
}

#[test]
fn a_repair_that_fails_leaves_the_log_refused() {
    if agentkit_sys::effective_uid().unwrap() == 0 {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let _unlock = Unlock(root.path().to_path_buf());
    symlink(outside.path(), root.path().join(".sandbox")).unwrap();
    std::fs::set_permissions(root.path(), Permissions::from_mode(0o555)).unwrap();
    let refused = OpLog::create(root.path(), "op-1", BYTES_4);
    assert!(refused.is_err());
    assert!(
        std::fs::symlink_metadata(root.path().join(".sandbox"))
            .unwrap()
            .is_symlink()
    );
    assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
}

#[test]
fn a_healthy_log_directory_is_left_alone() {
    let root = tempfile::tempdir().unwrap();
    log_in(root.path(), "op-1", BYTES_4);
    log_in(root.path(), "op-2", BYTES_4);
    assert!(set_aside_entries(root.path()).is_empty());
    assert!(root.path().join(".sandbox/logs/op-1.log").is_file());
}
