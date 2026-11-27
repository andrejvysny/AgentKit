use std::process::{Child, Command};

use super::*;
use crate::naming::WorkspaceId;

fn sleeper() -> Child {
    Command::new("/bin/sleep").arg("60").spawn().unwrap()
}

/// The pid of a child that recovery reaps itself (by raw `waitpid`).
fn pid_of(child: Child) -> u32 {
    child.id()
}

fn started(pid: u32) -> u64 {
    match lookup(pid).unwrap() {
        Lookup::Found { started, .. } => started,
        Lookup::Gone => panic!("pid {pid} not found"),
    }
}

fn uid() -> u32 {
    agentkit_sys::effective_uid().unwrap()
}

#[tokio::test]
async fn a_recorded_helper_that_is_ours_is_killed_and_proven_stopped() {
    let scratch = tempfile::tempdir().unwrap();
    let state = StateStore::open(scratch.path()).unwrap();
    let sockets = SocketDirs::new(scratch.path().join("sockets"), uid());
    let pid = pid_of(sleeper());
    let ws = WorkspaceId::parse("ws").unwrap();
    let helper = HelperRecord {
        generation: 3,
        pid,
        started: started(pid),
    };
    state.record_created(&ws, 3).await.unwrap();
    state.record_helper(&ws, helper).await.unwrap();
    assert_eq!(recover(&state, &sockets).await.unwrap(), 1);
    assert!(
        !matches!(lookup(pid).unwrap(), Lookup::Found { started, .. } if started == helper.started),
        "the helper is gone"
    );
    let record = state.get(&ws).await;
    assert_eq!((record.stopped_through, record.helper), (3, None));
    assert_eq!(recover(&state, &sockets).await.unwrap(), 0, "nothing left");
}

#[test]
fn a_pid_with_another_start_time_is_never_killed() {
    let mut child = sleeper();
    let pid = child.id();
    let other = HelperRecord {
        generation: 1,
        pid,
        started: started(pid).wrapping_add(1),
    };
    reclaim(other).unwrap();
    assert!(
        child.try_wait().unwrap().is_none(),
        "a process that is not the recorded helper keeps running"
    );
    child.kill().unwrap();
    child.wait().unwrap();
}

#[test]
fn a_free_pid_is_gone_already() {
    let mut child = Command::new("/bin/sh")
        .args(["-c", "exit 0"])
        .spawn()
        .unwrap();
    let pid = child.id();
    child.wait().unwrap();
    let helper = HelperRecord {
        generation: 1,
        pid,
        started: 1,
    };
    reclaim(helper).unwrap();
}
