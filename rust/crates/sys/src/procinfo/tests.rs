use std::fs::File;
use std::process::{Command, Stdio};

use super::*;

#[test]
fn this_process_has_a_stable_start_time_and_pid_zero_is_gone() {
    let pid = std::process::id();
    let first = lookup_process(pid).unwrap();
    let ProcessLookup::Found { started, zombie } = first else {
        panic!("own pid not found: {first:?}");
    };
    assert!(started > 0 && !zombie);
    assert_eq!(lookup_process(pid).unwrap(), first);
    assert_eq!(lookup_process(0).unwrap(), ProcessLookup::Gone);
}

#[test]
fn a_child_differs_from_its_parent_until_it_is_reaped() {
    let mut child = Command::new("/bin/sleep").arg("30").spawn().unwrap();
    let pid = child.id();
    assert!(matches!(
        lookup_process(pid).unwrap(),
        ProcessLookup::Found { zombie: false, .. }
    ));
    child.kill().unwrap();
    child.wait().unwrap();
    // Reaped: gone, unless the pid was reused already, which a caller
    // tells apart by the start time.
    if let ProcessLookup::Found { zombie, .. } = lookup_process(pid).unwrap() {
        assert!(!zombie, "a reaped child is no zombie");
    }
}

#[test]
fn the_holders_of_a_file_are_this_process_and_its_children_while_open() {
    // Other tests of this binary fork children that hold this process's
    // descriptors until they exec, so only the pids of this test's own
    // holders are asserted.
    let holders = |path: &Path| pids_holding(path, 16).unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let path = scratch.path().join("disk.img");
    std::fs::write(&path, b"disk").unwrap();
    let own = std::process::id();
    assert!(!holders(&path).contains(&own));
    let held = File::open(&path).unwrap();
    assert!(holders(&path).contains(&own));
    drop(held);
    assert!(!holders(&path).contains(&own));
    let mut child = Command::new("/bin/sleep")
        .arg("30")
        .stdin(Stdio::from(File::open(&path).unwrap()))
        .spawn()
        .unwrap();
    let pid = child.id();
    let found = holders(&path);
    assert!(found.contains(&pid) && !found.contains(&own), "{found:?}");
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(!holders(&path).contains(&pid), "released");
    let missing = scratch.path().join("missing.img");
    assert!(holders(&missing).is_empty());
}

#[test]
fn a_proc_tree_is_searched_by_device_and_inode() {
    use std::os::unix::fs::symlink;
    let scratch = tempfile::tempdir().unwrap();
    let (target, other) = (scratch.path().join("disk"), scratch.path().join("other"));
    std::fs::write(&target, b"disk").unwrap();
    std::fs::write(&other, b"other").unwrap();
    let proc_root = scratch.path().join("proc");
    for (pid, fd, to) in [
        ("12", "3", &target),
        ("34", "0", &other),
        ("56", "7", &target),
    ] {
        let fds = proc_root.join(pid).join("fd");
        std::fs::create_dir_all(&fds).unwrap();
        symlink(to, fds.join(fd)).unwrap();
    }
    std::fs::create_dir_all(proc_root.join("self/fd")).unwrap();
    std::fs::create_dir_all(proc_root.join("78")).unwrap();
    let mut found = holders_in(&proc_root, &target, 16).unwrap();
    found.sort_unstable();
    assert_eq!(found, vec![12, 56]);
    assert_eq!(holders_in(&proc_root, &target, 1).unwrap().len(), 1);
    let missing = scratch.path().join("missing");
    assert!(holders_in(&proc_root, &missing, 16).unwrap().is_empty());
}

/// Calls of [`pids_holding`] that missed the holder while it held the
/// file and opened and closed other files all along.
fn misses_under_churn(holder: u32, path: &Path, calls: usize) -> usize {
    (0..calls)
        .filter(|_| !pids_holding(path, 16).unwrap().contains(&holder))
        .count()
}

#[test]
fn a_holder_is_seen_while_it_opens_and_closes_other_files() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    let scratch = tempfile::tempdir().unwrap();
    let path = scratch.path().join("disk.img");
    std::fs::write(&path, b"disk").unwrap();
    let _held = File::open(&path).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let churn: Vec<_> = (0..4)
        .map(|n| {
            let (stop, other) = (Arc::clone(&stop), scratch.path().join(format!("churn-{n}")));
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    drop(File::create(&other).unwrap());
                }
            })
        })
        .collect();
    let own = misses_under_churn(std::process::id(), &path, 300);
    stop.store(true, Ordering::Relaxed);
    churn.into_iter().for_each(|t| t.join().unwrap());
    let mut child = Command::new("/bin/sh")
        .args(["-c", "exec 3<\"$0\"; while :; do : < /etc/hosts; done"])
        .arg(&path)
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(100));
    let other = misses_under_churn(child.id(), &path, 300);
    child.kill().unwrap();
    child.wait().unwrap();
    assert_eq!((own, other), (0, 0), "misses: own process, child");
}
