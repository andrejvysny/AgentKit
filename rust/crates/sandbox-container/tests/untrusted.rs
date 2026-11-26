//! Guest replies are untrusted (ADR 0039): a reply that breaks a bound or
//! a rule makes the machine unreachable, no request ever hangs, an exec
//! the guest keeps "running" past its deadline is cut off, and usage the
//! guest could not fully measure is taken from the launcher.

mod support;

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agentkit_egress::EgressPorts;
use agentkit_sandbox_container::{ContainerBackend, ContainerBackendConfig};
use agentkit_sandbox_proto::guest::{Entry, EntryKind, OperationState, OutputData, Reply, Request};
use agentkit_sandbox_proto::launcher::LauncherErrorCode;
use agentkit_testkit::ManualClock;
use agentkit_workspaces::{
    EXEC_TIMEOUT_DEFAULT_MS, Generation, ListEntries, MachineHandle, OBSERVE_CHUNK_MAX_BYTES,
    OperationId, OperationKind, OperationRequest, OutputLimit, PathRoot, RelPath, SandboxBackend,
    SandboxError, Sha256, StopProof, WorkspaceId,
};
use support::guest::{Action, Script, hello, reply, reply_with, starter};
use support::launcher::FakeLauncher;
use support::{DIGEST, backend_with, spec};
use tempfile::TempDir;

struct Fixture {
    dir: TempDir,
    launcher: FakeLauncher,
    backend: ContainerBackend,
    m: MachineHandle,
}

/// A started machine whose guest greets honestly, acks every exec as
/// running (as new, every time) and answers everything else with `rest`.
async fn machine(rest: impl Fn(u64, &Request) -> Action + Send + Sync + 'static) -> Fixture {
    machine_with(rest, EgressPorts::system(), false).await
}

/// [`machine`] with the egress ports (and so the clock) given; with
/// `replays`, an exec id seen before is acked as `existing`.
async fn machine_with(
    rest: impl Fn(u64, &Request) -> Action + Send + Sync + 'static,
    ports: EgressPorts,
    replays: bool,
) -> Fixture {
    let started = Mutex::new(HashSet::new());
    let script: Script = Arc::new(move |id, peer, request| match request {
        Request::Hello { .. } => hello(id, peer, DIGEST),
        Request::Exec(exec) => {
            let repeated = !started.lock().unwrap().insert(exec.op.clone());
            Action::Send(vec![reply(
                id,
                Reply::Accepted {
                    state: OperationState::Running,
                    existing: replays && repeated,
                },
            )])
        }
        other => rest(id, other),
    });
    let dir = tempfile::tempdir().unwrap();
    let tune = |config: &mut ContainerBackendConfig| {
        config.request_timeout = Duration::from_millis(300);
    };
    let (launcher, backend) = backend_with(dir.path(), starter(script), tune, ports);
    let m = backend.ensure_machine(&spec("ws-1", 1)).await.unwrap();
    Fixture {
        dir,
        launcher,
        backend,
        m,
    }
}

fn op() -> OperationId {
    OperationId::new("op-1")
}

fn listing() -> ListEntries {
    ListEntries {
        root: PathRoot::Workspace,
        path: RelPath::parse("tree").unwrap(),
        depth: 2,
        max_entries: 10,
    }
}

fn unavailable<T: std::fmt::Debug>(outcome: Result<T, SandboxError>) {
    assert!(
        matches!(outcome, Err(SandboxError::Unavailable { .. })),
        "expected unavailable, got {outcome:?}"
    );
}

/// The machine answers nothing any more, whatever is asked, and its
/// generation is stale to `ensure_machine` (so the service proves it
/// stopped and moves on); that way out works: a proven stop, then the
/// next generation.
async fn assert_unreachable(f: &Fixture) {
    unavailable(f.backend.list(&f.m, &listing()).await);
    unavailable(f.backend.drain_denials(&f.m, 0).await);
    assert_eq!(
        f.backend.ensure_machine(&spec("ws-1", 1)).await,
        Err(SandboxError::StaleGeneration {
            requested: Generation(1),
            current: Generation(1),
        })
    );
    let ws = WorkspaceId::new("ws-1");
    let proof = f.backend.stop_machine(&ws, Generation(1)).await;
    assert_eq!(proof, Ok(StopProof::Stopped));
    let next = f.backend.ensure_machine(&spec("ws-1", 2)).await.unwrap();
    assert_eq!(next.generation, Generation(2));
}

fn output(state: OperationState, next_seq: u64, len: u64, eof: bool) -> Reply {
    Reply::OutputData(OutputData {
        state,
        next_seq,
        truncated: false,
        eof,
        len,
    })
}

#[tokio::test]
async fn output_breaking_its_bounds_makes_the_machine_unreachable() {
    let running = OperationState::Running;
    let cases: [(Reply, &[u8]); 4] = [
        (output(running, 0, OBSERVE_CHUNK_MAX_BYTES + 1, false), b""),
        (output(running, 3, 0, false), b""),
        (output(running, 12, 5, false), b"12345"),
        (output(running, 10, 0, true), b""),
    ];
    for (header, bytes) in cases {
        let f = machine(move |id, request| match request {
            Request::Output { .. } => Action::Send(reply_with(id, header.clone(), bytes)),
            _ => Action::Silent,
        })
        .await;
        unavailable(f.backend.observe(&f.m, &op(), 10).await);
        assert_unreachable(&f).await;
    }
}

#[tokio::test]
async fn a_receipt_that_changes_makes_the_machine_unreachable() {
    let calls = Arc::new(AtomicUsize::new(0));
    let f = machine(move |id, request| match request {
        Request::Output { after_seq, .. } => {
            let state = if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                OperationState::Succeeded { exit: 0 }
            } else {
                OperationState::Failed {
                    exit: Some(1),
                    signal: None,
                }
            };
            Action::Send(reply_with(id, output(state, *after_seq, 0, true), b""))
        }
        _ => Action::Silent,
    })
    .await;
    let first = f.backend.observe(&f.m, &op(), 0).await.unwrap();
    assert_eq!(
        first.state,
        agentkit_workspaces::OperationState::Succeeded { exit: 0 }
    );
    unavailable(f.backend.observe(&f.m, &op(), 0).await);
    assert_unreachable(&f).await;
}

fn exec(id: &str) -> OperationRequest {
    exec_with_timeout(id, EXEC_TIMEOUT_DEFAULT_MS)
}

fn exec_with_timeout(id: &str, timeout_ms: u64) -> OperationRequest {
    OperationRequest {
        id: OperationId::new(id),
        workspace_id: WorkspaceId::new("ws-1"),
        generation: Generation(1),
        args_sha256: Sha256::of(b"true"),
        kind: OperationKind::Exec {
            command: "true".to_owned(),
            cwd: RelPath::root(),
            timeout_ms,
            output_limit: OutputLimit::LOG,
        },
    }
}

#[tokio::test]
async fn an_operation_started_twice_makes_the_machine_unreachable() {
    let f = machine(|_, _| Action::Silent).await;
    let ack = f.backend.dispatch(&f.m, &exec("op-1")).await.unwrap();
    assert!(!ack.existing);
    unavailable(f.backend.dispatch(&f.m, &exec("op-1")).await);
    assert_unreachable(&f).await;
}

#[tokio::test]
async fn entries_outside_the_listing_make_the_machine_unreachable() {
    let entry = |path: &str| Entry {
        path: path.to_owned(),
        kind: EntryKind::File,
        size: Some(1),
    };
    for bad in [
        "../etc/passwd",
        "/etc/passwd",
        "other/a.txt",
        "tree/a/b/c.txt",
    ] {
        let listed = entry(bad);
        let f = machine(move |id, request| match request {
            Request::List(_) => Action::Send(vec![reply(
                id,
                Reply::Entries {
                    entries: vec![listed.clone()],
                    truncated: false,
                },
            )]),
            _ => Action::Silent,
        })
        .await;
        unavailable(f.backend.list(&f.m, &listing()).await);
        assert_unreachable(&f).await;
    }
}

#[tokio::test]
async fn an_unanswered_request_times_out_and_the_machine_stays() {
    let f = machine(|id, request| match request {
        Request::List(_) => Action::Send(vec![reply(
            id,
            Reply::Entries {
                entries: Vec::new(),
                truncated: false,
            },
        )]),
        _ => Action::Silent,
    })
    .await;
    let started = Instant::now();
    unavailable(f.backend.observe(&f.m, &op(), 0).await);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "observe is bounded"
    );
    let listing = f.backend.list(&f.m, &listing()).await.unwrap();
    assert!(listing.entries.is_empty(), "a timeout alone breaks nothing");
}

#[tokio::test]
async fn a_closed_channel_makes_the_machine_unreachable() {
    let f = machine(|_, request| match request {
        Request::Output { .. } => Action::Close,
        _ => Action::Silent,
    })
    .await;
    let started = Instant::now();
    unavailable(f.backend.observe(&f.m, &op(), 0).await);
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_unreachable(&f).await;
}

/// The exec deadline (S2): an exec the guest still reports running after
/// its `timeout_ms` plus 30 s on the backend's clock is cancelled and the
/// machine made unreachable, so the service's observe fails and the
/// operation becomes uncertain. A replay does not extend the deadline, and
/// an operation that reports its end late is still settled normally.
#[tokio::test]
async fn an_exec_running_past_its_deadline_makes_the_machine_unreachable() {
    let clock = ManualClock::new(1_000_000);
    let ports = EgressPorts {
        runtime: clock.clone(),
        ..EgressPorts::system()
    };
    let cancels = Arc::new(Mutex::new(Vec::new()));
    let seen = cancels.clone();
    let f = machine_with(
        move |id, request| match request {
            Request::Output { op, after_seq, .. } => {
                let state = if op == "op-late" {
                    OperationState::Succeeded { exit: 0 }
                } else {
                    OperationState::Running
                };
                let eof = op == "op-late";
                Action::Send(reply_with(id, output(state, *after_seq, 0, eof), b""))
            }
            Request::Cancel { op } => {
                seen.lock().unwrap().push(op.clone());
                Action::Send(vec![reply(
                    id,
                    Reply::State {
                        state: OperationState::Running,
                    },
                )])
            }
            _ => Action::Silent,
        },
        ports,
        true,
    )
    .await;
    let timeout_ms = 1_000;
    for id in ["op-1", "op-late"] {
        let op = exec_with_timeout(id, timeout_ms);
        assert!(!f.backend.dispatch(&f.m, &op).await.unwrap().existing);
    }
    let running = agentkit_workspaces::OperationState::Running;
    assert_eq!(
        f.backend.observe(&f.m, &op(), 0).await.unwrap().state,
        running
    );
    // A replay later on keeps the first dispatch's deadline.
    clock.advance(20_000);
    let replay = exec_with_timeout("op-1", timeout_ms);
    assert!(f.backend.dispatch(&f.m, &replay).await.unwrap().existing);
    clock.advance(1_000 + 30_000 - 20_000 - 1);
    assert_eq!(
        f.backend.observe(&f.m, &op(), 0).await.unwrap().state,
        running
    );
    assert!(cancels.lock().unwrap().is_empty());
    clock.advance(1);
    let late = f
        .backend
        .observe(&f.m, &OperationId::new("op-late"), 0)
        .await
        .unwrap();
    assert_eq!(
        late.state,
        agentkit_workspaces::OperationState::Succeeded { exit: 0 },
        "an end reported late still settles"
    );
    unavailable(f.backend.observe(&f.m, &op(), 0).await);
    assert_eq!(
        *cancels.lock().unwrap(),
        ["op-1"],
        "the guest was told to cancel"
    );
    assert_unreachable(&f).await;
}

fn usage_reply(id: u64, complete: bool) -> Action {
    Action::Send(vec![reply(
        id,
        Reply::Usage {
            workspace_bytes: 7,
            home_bytes: 8,
            complete,
        },
    )])
}

/// Usage the guest measured completely is used as is; incomplete usage is
/// replaced by the launcher's, and when the launcher cannot answer either
/// the call fails (never 0).
#[tokio::test]
async fn incomplete_guest_usage_falls_back_to_the_launcher_and_fails_closed() {
    let complete = Arc::new(AtomicUsize::new(1));
    let flag = complete.clone();
    let f = machine(move |id, request| match request {
        Request::Usage => usage_reply(id, flag.load(Ordering::SeqCst) == 1),
        _ => Action::Silent,
    })
    .await;
    let ws = WorkspaceId::new("ws-1");
    let usage_calls = |f: &Fixture| {
        f.launcher
            .calls()
            .iter()
            .filter(|call| call.starts_with("usage "))
            .count()
    };
    assert_eq!(f.backend.storage_usage(&ws).await.unwrap().used_bytes, 15);
    assert_eq!(
        usage_calls(&f),
        0,
        "complete guest figures need no launcher"
    );

    complete.store(0, Ordering::SeqCst);
    let data = f.dir.path().join("data/ws-1/workspace");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(data.join("big.bin"), vec![0u8; 4321]).unwrap();
    assert_eq!(f.backend.storage_usage(&ws).await.unwrap().used_bytes, 4321);
    assert_eq!(usage_calls(&f), 1);

    for code in [
        LauncherErrorCode::RuntimeFailed,
        LauncherErrorCode::NotFound,
    ] {
        f.launcher.fail_next_usage(code);
        let outcome = f.backend.storage_usage(&ws).await;
        assert!(outcome.is_err(), "{code:?}: fails closed, got {outcome:?}");
    }
}

/// The cut-off does not depend on the guest answering the cancel: a guest
/// that ignores it is closed after the cancel wait, and a caller that stops
/// waiting during the cancel still leaves the machine unreachable.
#[tokio::test]
async fn an_overdue_exec_is_cut_off_even_when_the_cancel_goes_unanswered() {
    for caller_gives_up in [false, true] {
        let clock = ManualClock::new(1_000_000);
        let ports = EgressPorts {
            runtime: clock.clone(),
            ..EgressPorts::system()
        };
        let f = machine_with(
            |id, request| match request {
                Request::Output { after_seq, .. } => Action::Send(reply_with(
                    id,
                    output(OperationState::Running, *after_seq, 0, false),
                    b"",
                )),
                _ => Action::Silent,
            },
            ports,
            false,
        )
        .await;
        f.backend
            .dispatch(&f.m, &exec_with_timeout("op-1", 1_000))
            .await
            .unwrap();
        clock.advance(1_000 + 30_000);
        let id = op();
        let observed = f.backend.observe(&f.m, &id, 0);
        if caller_gives_up {
            // Shorter than the cancel wait (the 300 ms request time).
            let outcome = tokio::time::timeout(Duration::from_millis(50), observed).await;
            assert!(outcome.is_err(), "the caller stopped waiting: {outcome:?}");
        } else {
            let started = Instant::now();
            unavailable(observed.await);
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "the cancel wait is bounded"
            );
        }
        assert_unreachable(&f).await;
    }
}
