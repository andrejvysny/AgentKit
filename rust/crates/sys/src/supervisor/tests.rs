use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::*;
use crate::fs::open_dir_nofollow;
use crate::process::{own_process_group, peek_child_exit};

/// The pid of a spawned child whose exit these tests collect with
/// `waitpid` themselves; std's handle is never waited on.
fn spawned(cmd: &mut Command) -> u32 {
    pid_of(cmd.spawn().unwrap())
}

fn pid_of(child: std::process::Child) -> u32 {
    child.id()
}

fn wait_reaped(pid: u32) -> ChildExit {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match reap_child(pid).expect("reap own child") {
            ChildExit::Running => {
                assert!(Instant::now() < deadline, "child never ended");
                std::thread::sleep(Duration::from_millis(5));
            }
            other => return other,
        }
    }
}

#[test]
fn kill_all_processes_is_refused_outside_pid_1() {
    assert!(!is_init(), "the test runner is never PID 1");
    let error = kill_all_processes(Signal::Kill).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
}

#[test]
fn new_session_child_runs_in_the_given_directory() {
    let root = tempfile::tempdir().unwrap();
    let canonical = std::fs::canonicalize(root.path()).unwrap();
    let dir = open_dir_nofollow(root.path()).unwrap();
    let mut cmd = Command::new("/bin/sh");
    cmd.args(["-c", "pwd -P > where.txt"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    spawn_in_new_session(&mut cmd, dir).unwrap();
    let pid = spawned(&mut cmd);
    drop(cmd);
    assert_eq!(wait_reaped(pid), ChildExit::Exited { code: 0 });
    let written = std::fs::read_to_string(canonical.join("where.txt")).unwrap();
    assert_eq!(written.trim(), canonical.to_str().unwrap());
}

#[test]
fn new_session_child_leads_its_own_group() {
    let root = tempfile::tempdir().unwrap();
    let dir = open_dir_nofollow(root.path()).unwrap();
    let mut cmd = Command::new("/bin/sleep");
    cmd.arg("30");
    spawn_in_new_session(&mut cmd, dir).unwrap();
    let pid = spawned(&mut cmd);
    drop(cmd);
    assert_ne!(pid, own_process_group());
    let group = crate::ProcessGroup::from_leader_pid(pid).unwrap();
    assert!(!group.is_gone(), "a group numbered after the child exists");
    group.signal(Signal::Kill).unwrap();
    assert_eq!(
        wait_reaped(pid),
        ChildExit::Killed {
            signal: libc::SIGKILL
        }
    );
}

#[test]
fn reap_child_reports_exit_signal_and_absence() {
    let child = spawned(Command::new("/bin/sh").args(["-c", "exit 7"]));
    assert_eq!(wait_reaped(child), ChildExit::Exited { code: 7 });
    assert_eq!(reap_child(child).unwrap(), ChildExit::NoSuchChild);
    let sleeper = spawned(Command::new("/bin/sleep").arg("30"));
    assert_eq!(reap_child(sleeper).unwrap(), ChildExit::Running);
    crate::signal_process(sleeper, Signal::Kill).unwrap();
    let killed = wait_reaped(sleeper);
    assert_eq!(
        killed,
        ChildExit::Killed {
            signal: libc::SIGKILL
        }
    );
    assert_eq!(reap_child(0).unwrap(), ChildExit::NoSuchChild);
    assert_eq!(reap_child(u32::MAX).unwrap(), ChildExit::NoSuchChild);
}

#[test]
fn peeked_exit_stays_reapable() {
    let child = spawned(Command::new("/bin/sh").args(["-c", "exit 4"]));
    let deadline = Instant::now() + Duration::from_secs(10);
    while peek_child_exit(child).unwrap() == ChildExit::Running {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(wait_reaped(child), ChildExit::Exited { code: 4 });
}

#[test]
fn stat_text_parsing_survives_odd_command_names() {
    assert_eq!(parse_stat_ppid("42 (bash) S 7 42 42 0"), Some(7));
    assert_eq!(parse_stat_ppid("43 (a) (b c) R 9 1 1"), Some(9));
    assert_eq!(parse_stat_ppid("44 (x)"), None);
    assert_eq!(parse_stat_ppid("garbage"), None);
}

#[test]
fn below_walks_the_whole_subtree_only() {
    let links = [(2, 1), (3, 2), (4, 3), (5, 1), (6, 9), (1, 0)];
    let mut found = below(2, &links);
    found.sort_unstable();
    assert_eq!(found, [3, 4]);
    let mut all = below(1, &links);
    all.sort_unstable();
    assert_eq!(all, [2, 3, 4, 5]);
}

#[cfg(target_os = "linux")]
#[test]
fn descendants_lists_a_spawned_child() {
    let mut child = Command::new("/bin/sleep").arg("30").spawn().unwrap();
    let found = descendants(std::process::id()).unwrap();
    assert!(found.contains(&child.id()), "{found:?}");
    child.kill().unwrap();
    child.wait().unwrap();
}

#[cfg(not(target_os = "linux"))]
#[test]
fn linux_only_primitives_refuse_elsewhere() {
    for result in [set_non_dumpable(), set_child_subreaper()] {
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Unsupported);
    }
    let error = descendants(std::process::id()).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::Unsupported);
}
