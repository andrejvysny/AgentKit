//! The supervisor in [`ProcessMode::Supervisor`]: it reaps every child of
//! its process, so it runs alone in this test binary. Exercises the
//! reap-all path that PID 1 uses: exit statuses routed to their
//! operations, background processes ended, cancel, shutdown.

mod support;

use std::time::Duration;

use agentkit_sandbox_proto::guest::{OperationState, Reply, Request};
use agentkit_sandboxd::{Exit, ProcessMode};
use support::{Guest, assert_new, exec_request, failed, path_exists, start_in};

#[tokio::test]
async fn the_reap_all_path_serves_commands_end_to_end() {
    // As in the binary: on Linux the process becomes a child subreaper, so
    // orphans stay descendants for the sweep.
    agentkit_sandboxd::harden_process(ProcessMode::Supervisor).unwrap();
    let guest = Guest::new("PATH=/usr/bin:/bin\n");
    let mut host = start_in(&guest, ProcessMode::Supervisor).await;
    let hi = host.run(exec_request("op-1", "printf hi")).await;
    assert_eq!(
        (hi.state, hi.output.as_slice()),
        (OperationState::Succeeded { exit: 0 }, &b"hi"[..])
    );
    assert_eq!(
        host.run(exec_request("op-2", "exit 3")).await.state,
        failed(3)
    );
    let background = "(sleep 1; printf late > late.txt) & exit 0";
    let done = host.run(exec_request("op-3", background)).await;
    assert_eq!(done.state, OperationState::Succeeded { exit: 0 });
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(!path_exists(&guest.workspace.join("late.txt")));
    assert_new(&host.exec(exec_request("op-4", "sleep 3600")).await);
    host.request(Request::Cancel { op: "op-4".into() }).await;
    assert_eq!(host.finish("op-4").await.state, OperationState::Cancelled);
    assert_new(&host.exec(exec_request("op-5", "sleep 3600")).await);
    let (reply, exit) = host.shutdown().await;
    assert_eq!((reply, exit), (Reply::Bye, Exit::Shutdown));
}
