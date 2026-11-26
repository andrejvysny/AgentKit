//! Commands over the protocol, against a supervisor in this process
//! (Embedded mode): receipts, idempotence, cancel, deadline, output limit,
//! background processes, environment and working directory.

mod support;

use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};

use agentkit_sandbox_proto::guest::{
    GuestErrorCode, OperationState, OutputLimit, PathRoot, Reply, ReplyEnvelope, Request,
};
use agentkit_sandbox_proto::{Frame, parse_control};
use support::{Guest, assert_new, exec_request, failed, path_exists, read_request, start};

const ENV: &str = "PATH=/usr/bin:/bin\nFROM_FILE=yes\n";

#[tokio::test]
async fn exit_status_and_output_are_reported_promptly() {
    let guest = Guest::new(ENV);
    let mut host = start(&guest).await;
    let began = Instant::now();
    let hi = host.run(exec_request("op-1", "printf hi")).await;
    assert_eq!(hi.state, OperationState::Succeeded { exit: 0 });
    assert_eq!(hi.output, b"hi");
    assert!(
        began.elapsed() < Duration::from_millis(1500),
        "the receipt does not wait out the drain bound: {:?}",
        began.elapsed()
    );
    let three = host.run(exec_request("op-2", "exit 3")).await;
    assert_eq!(three.state, failed(3));
    let both = host
        .run(exec_request("op-3", "printf out; printf err >&2"))
        .await;
    assert_eq!(both.output, b"outerr", "stdout and stderr share the log");
    let log = std::fs::read(guest.workspace.join(".sandbox/logs/op-3.log")).unwrap();
    assert_eq!(log, b"outerr");
    let (_, exit) = host.shutdown().await;
    assert_eq!(exit, agentkit_sandboxd::Exit::Shutdown);
}

#[tokio::test]
async fn same_id_and_args_runs_once_other_args_conflict() {
    let guest = Guest::new(ENV);
    let mut host = start(&guest).await;
    let mark = exec_request("op-1", "printf x >> mark.txt");
    assert_new(&host.exec(mark.clone()).await);
    let again = host.exec(mark.clone()).await;
    assert!(
        matches!(again, Reply::Accepted { existing: true, .. }),
        "{again:?}"
    );
    let done = host.finish("op-1").await;
    assert_eq!(done.state, OperationState::Succeeded { exit: 0 });
    let replay = host.exec(mark).await;
    assert_eq!(
        replay,
        Reply::Accepted {
            state: done.state,
            existing: true
        }
    );
    assert_eq!(
        std::fs::read(guest.workspace.join("mark.txt")).unwrap(),
        b"x"
    );
    let other = host.exec(exec_request("op-1", "exit 3")).await;
    assert_eq!(other, Reply::Conflict);
}

#[tokio::test]
async fn unknown_operations_are_unknown() {
    let guest = Guest::new(ENV);
    let mut host = start(&guest).await;
    assert_eq!(host.status("never").await, Reply::Unknown);
    let cancel = host.request(Request::Cancel { op: "never".into() }).await;
    assert_eq!(cancel, Reply::Unknown);
    let output = host
        .request(Request::Output {
            op: "never".into(),
            after_seq: 0,
            max_bytes: 10,
        })
        .await;
    assert_eq!(output, Reply::Unknown);
}

#[tokio::test]
async fn a_receipt_and_its_output_never_change() {
    let guest = Guest::new(ENV);
    let mut host = start(&guest).await;
    let done = host.run(exec_request("op-1", "printf hi")).await;
    for _ in 0..3 {
        let (seen, bytes) = host.output("op-1", 0, 1 << 20).await;
        assert_eq!((seen.state, bytes.as_slice()), (done.state, &b"hi"[..]));
        assert!(seen.eof);
        let (tail, rest) = host.output("op-1", seen.next_seq, 1 << 20).await;
        assert!(rest.is_empty() && tail.eof && tail.next_seq == seen.next_seq);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn observe_never_blocks_and_cancel_ends_as_cancelled() {
    let guest = Guest::new(ENV);
    let mut host = start(&guest).await;
    assert_new(&host.exec(exec_request("op-1", "sleep 3600")).await);
    for _ in 0..3 {
        let (seen, _) = host.output("op-1", 0, 1024).await;
        assert!(!seen.eof, "{seen:?}");
        assert!(matches!(
            seen.state,
            OperationState::Accepted | OperationState::Running
        ));
    }
    let cancel = host.request(Request::Cancel { op: "op-1".into() }).await;
    assert!(matches!(cancel, Reply::State { .. }), "{cancel:?}");
    assert_eq!(host.finish("op-1").await.state, OperationState::Cancelled);
    let again = host.request(Request::Cancel { op: "op-1".into() }).await;
    assert_eq!(
        again,
        Reply::State {
            state: OperationState::Cancelled
        }
    );
}

#[tokio::test]
async fn a_command_ignoring_sigterm_is_killed_after_the_grace() {
    let guest = Guest::new(ENV);
    let mut host = start(&guest).await;
    let stubborn = "trap '' TERM; while :; do sleep 0.1; done";
    assert_new(&host.exec(exec_request("op-1", stubborn)).await);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let began = Instant::now();
    host.request(Request::Cancel { op: "op-1".into() }).await;
    assert_eq!(host.finish("op-1").await.state, OperationState::Cancelled);
    assert!(
        began.elapsed() >= Duration::from_millis(1500),
        "{:?}",
        began.elapsed()
    );
}

#[tokio::test]
async fn the_deadline_ends_the_command_as_timed_out() {
    let guest = Guest::new(ENV);
    let mut host = start(&guest).await;
    let req = agentkit_sandbox_proto::guest::ExecRequest {
        timeout_ms: 200,
        ..exec_request("op-1", "sleep 3600")
    };
    assert_eq!(host.run(req).await.state, OperationState::TimedOut);
}

#[tokio::test]
async fn endless_output_is_bounded_by_the_limit() {
    let guest = Guest::new(ENV);
    let mut host = start(&guest).await;
    let small = OutputLimit {
        max_bytes: 4096,
        max_lines: None,
    };
    let req = agentkit_sandbox_proto::guest::ExecRequest {
        output_limit: small,
        timeout_ms: 60_000,
        ..exec_request("op-1", "yes")
    };
    assert_new(&host.exec(req).await);
    let done = host.finish_bounded("op-1", small.max_bytes).await;
    assert_eq!(done.state, OperationState::OutputLimit);
    assert!(done.truncated, "dropped output is reported");
    assert_eq!(done.output.len(), 4096);
    let log = guest.workspace.join(".sandbox/logs/op-1.log");
    assert_eq!(
        std::fs::metadata(log).unwrap().len(),
        4096,
        "the log never overshoots"
    );
}

#[tokio::test]
async fn the_line_limit_keeps_whole_lines() {
    let guest = Guest::new(ENV);
    let mut host = start(&guest).await;
    let lines = OutputLimit {
        max_bytes: 1024,
        max_lines: Some(2),
    };
    let req = agentkit_sandbox_proto::guest::ExecRequest {
        output_limit: lines,
        ..exec_request("op-1", "printf 'a\\nb\\nc\\n'")
    };
    let done = host.run(req).await;
    assert_eq!(
        (done.state, done.output.as_slice()),
        (OperationState::OutputLimit, &b"a\nb\n"[..])
    );
    let exact = agentkit_sandbox_proto::guest::ExecRequest {
        output_limit: lines,
        ..exec_request("op-2", "printf 'a\\nb\\n'")
    };
    assert_eq!(
        host.run(exact).await.state,
        OperationState::Succeeded { exit: 0 }
    );
}

#[tokio::test]
async fn a_background_process_ends_with_its_command() {
    let guest = Guest::new(ENV);
    let mut host = start(&guest).await;
    let background = "(sleep 1; printf late > late.txt) & exit 0";
    let done = host.run(exec_request("op-1", background)).await;
    assert_eq!(done.state, OperationState::Succeeded { exit: 0 });
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(!path_exists(&guest.workspace.join("late.txt")));
    let read = host
        .read(read_request(PathRoot::Workspace, "late.txt"))
        .await;
    assert!(
        matches!(&read, Err(error) if error.code == GuestErrorCode::NotFound),
        "{read:?}"
    );
}

#[tokio::test]
async fn the_environment_is_the_file_plus_tz_and_stdin_is_empty() {
    let guest = Guest::new(ENV);
    let mut host = start(&guest).await;
    let probe = "printf '%s|%s|%s|' \"$FROM_FILE\" \"$TZ\" \"$CARGO_MANIFEST_DIR\"; cat";
    let req = agentkit_sandbox_proto::guest::ExecRequest {
        tz: Some("Europe/Bratislava".into()),
        ..exec_request("op-1", probe)
    };
    let done = host.run(req).await;
    assert_eq!(done.state, OperationState::Succeeded { exit: 0 });
    assert_eq!(done.output, b"yes|Europe/Bratislava||");
}

#[tokio::test]
async fn the_working_directory_is_made_below_the_workspace() {
    let guest = Guest::new(ENV);
    let mut host = start(&guest).await;
    let req = agentkit_sandbox_proto::guest::ExecRequest {
        cwd: "chats/c1".into(),
        ..exec_request("op-1", "pwd -P")
    };
    let done = host.run(req).await;
    let canonical = std::fs::canonicalize(guest.workspace.join("chats/c1")).unwrap();
    assert_eq!(
        String::from_utf8(done.output).unwrap().trim_end(),
        canonical.to_str().unwrap()
    );
}

#[tokio::test]
async fn a_working_directory_through_a_link_is_refused() {
    let guest = Guest::new(ENV);
    let outside = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), guest.workspace.join("escape")).unwrap();
    let mut host = start(&guest).await;
    let req = agentkit_sandbox_proto::guest::ExecRequest {
        cwd: "escape/sub".into(),
        ..exec_request("op-1", "printf ran > ran.txt")
    };
    let done = host.run(req).await;
    assert_eq!(done.state, failed(126));
    let note = String::from_utf8(done.output).unwrap();
    assert!(note.contains("symbolic link"), "{note}");
    assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
}

/// Entries of the workspace a log-directory repair set aside.
fn set_aside(guest: &Guest) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(&guest.workspace)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(".sandbox.broken-"))
        })
        .collect()
}

/// Gives every directory below a path back a usable mode when dropped (a
/// test locked some), so the scratch tree can be removed.
struct Unlock(std::path::PathBuf);

impl Drop for Unlock {
    fn drop(&mut self) {
        unlock(&self.0);
    }
}

fn unlock(dir: &std::path::Path) {
    let Ok(meta) = std::fs::symlink_metadata(dir) else {
        return;
    };
    if !meta.is_dir() {
        return;
    }
    let _ = std::fs::set_permissions(dir, Permissions::from_mode(0o755));
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            unlock(&entry.path());
        }
    }
}

#[tokio::test]
async fn a_linked_log_directory_is_set_aside_and_the_command_runs() {
    let guest = Guest::new(ENV);
    let outside = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), guest.workspace.join(".sandbox")).unwrap();
    let mut host = start(&guest).await;
    let done = host
        .run(exec_request("op-1", "printf ran > ran.txt; printf hi"))
        .await;
    assert_eq!(
        (done.state, done.output.as_slice()),
        (OperationState::Succeeded { exit: 0 }, &b"hi"[..])
    );
    assert!(path_exists(&guest.workspace.join("ran.txt")));
    assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    let aside = set_aside(&guest);
    assert_eq!(aside.len(), 1);
    assert_eq!(std::fs::read_link(&aside[0]).unwrap(), outside.path());
    let link_to_root = guest.workspace.join(".sandbox/logs");
    std::fs::remove_dir_all(&link_to_root).unwrap();
    std::os::unix::fs::symlink("/", &link_to_root).unwrap();
    let again = host.run(exec_request("op-2", "printf again")).await;
    assert_eq!(again.state, OperationState::Succeeded { exit: 0 });
    assert_eq!(again.output, b"again");
    assert_eq!(set_aside(&guest).len(), 2);
}

#[tokio::test]
async fn commands_started_together_repair_the_log_directory_once() {
    let guest = Guest::new(ENV);
    let outside = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), guest.workspace.join(".sandbox")).unwrap();
    let mut host = start(&guest).await;
    let ops = ["op-1", "op-2", "op-3", "op-4"];
    // Sent before any ack, so their log setups run side by side.
    let mut pending = Vec::new();
    for op in ops {
        let req = exec_request(op, &format!("printf {op}"));
        pending.push(host.send(Request::Exec(req)).await);
    }
    while !pending.is_empty() {
        if let Some(Frame::Control(json)) = host.next_frame().await {
            let reply: ReplyEnvelope = parse_control(&json).unwrap();
            if let Some(index) = pending.iter().position(|id| *id == reply.id) {
                pending.remove(index);
                assert_new(&reply.body);
            }
        }
    }
    for op in ops {
        let done = host.finish(op).await;
        assert_eq!(
            (done.state, done.output.as_slice()),
            (OperationState::Succeeded { exit: 0 }, op.as_bytes()),
        );
    }
    assert_eq!(
        set_aside(&guest).len(),
        1,
        "one repair, not one per command"
    );
    assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn a_log_directory_locked_by_the_workload_is_set_aside() {
    let guest = Guest::new(ENV);
    let _unlock = Unlock(guest.workspace.clone());
    let mut host = start(&guest).await;
    let lock = "chmod 000 .sandbox/logs; printf locked";
    let locked = host.run(exec_request("op-1", lock)).await;
    assert_eq!(locked.state, OperationState::Succeeded { exit: 0 });
    // Its own log becomes unreadable by name once it is closed, so only
    // the receipt is checked.
    let next = host.run(exec_request("op-2", "chmod 000 .sandbox")).await;
    assert_eq!(next.state, OperationState::Succeeded { exit: 0 });
    let after = host.run(exec_request("op-3", "printf after")).await;
    assert_eq!(
        (after.state, after.output.as_slice()),
        (OperationState::Succeeded { exit: 0 }, &b"after"[..])
    );
    assert_eq!(set_aside(&guest).len(), 2);
}

#[tokio::test]
async fn a_log_directory_that_cannot_be_set_aside_refuses_the_command() {
    if agentkit_sys::effective_uid().unwrap() == 0 {
        return;
    }
    let guest = Guest::new(ENV);
    let outside = tempfile::tempdir().unwrap();
    let marker = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), guest.workspace.join(".sandbox")).unwrap();
    std::fs::set_permissions(&guest.workspace, Permissions::from_mode(0o555)).unwrap();
    let mut host = start(&guest).await;
    let ran = marker.path().join("ran.txt");
    let command = format!("printf ran > '{}'", ran.display());
    let done = host.run(exec_request("op-1", &command)).await;
    std::fs::set_permissions(&guest.workspace, Permissions::from_mode(0o755)).unwrap();
    assert_eq!(done.state, failed(126));
    assert!(!path_exists(&ran), "it never ran");
    assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    let fixed = host.run(exec_request("op-2", "printf fixed")).await;
    assert_eq!(fixed.state, OperationState::Succeeded { exit: 0 });
}

#[tokio::test]
async fn malformed_commands_are_refused_and_not_recorded() {
    let guest = Guest::new(ENV);
    let mut host = start(&guest).await;
    let cases = [
        agentkit_sandbox_proto::guest::ExecRequest {
            op: ".hidden".into(),
            ..exec_request("x", "true")
        },
        agentkit_sandbox_proto::guest::ExecRequest {
            cwd: "../up".into(),
            ..exec_request("op-2", "true")
        },
        agentkit_sandbox_proto::guest::ExecRequest {
            timeout_ms: 0,
            ..exec_request("op-3", "true")
        },
        agentkit_sandbox_proto::guest::ExecRequest {
            output_limit: OutputLimit {
                max_bytes: (64 << 20) + 1,
                max_lines: None,
            },
            ..exec_request("op-4", "true")
        },
    ];
    for req in cases {
        let op = req.op.clone();
        let reply = host.exec(req).await;
        assert!(matches!(reply, Reply::Error(_)), "{reply:?}");
        assert_eq!(host.status(&op).await, Reply::Unknown);
    }
}

#[tokio::test]
async fn two_commands_in_flight_reach_their_own_receipts() {
    let guest = Guest::new(ENV);
    let mut host = start(&guest).await;
    assert_new(
        &host
            .exec(exec_request("op-1", "sleep 0.5; printf slow"))
            .await,
    );
    assert_new(&host.exec(exec_request("op-2", "printf fast; exit 4")).await);
    let fast = host.finish("op-2").await;
    assert_eq!(
        (fast.state, fast.output.as_slice()),
        (failed(4), &b"fast"[..])
    );
    let slow = host.finish("op-1").await;
    assert_eq!(
        (slow.state, slow.output.as_slice()),
        (OperationState::Succeeded { exit: 0 }, &b"slow"[..])
    );
}
