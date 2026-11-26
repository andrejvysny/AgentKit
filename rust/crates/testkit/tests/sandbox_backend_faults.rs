//! Fake-only behaviour of `FakeSandboxBackend`: each scripted fault, the
//! counters, and the rules the port leaves to the backend.

use std::sync::Arc;
use std::time::Duration;

use agentkit_testkit::{
    CONNECT_REFUSED_EXIT, DISK_FULL_EXIT, FakeSandboxBackend, ManualClock, Program, UNSCRIPTED_EXIT,
};
use agentkit_workspaces::{
    BackendDescriptor, ByteSource, FileMode, Generation, MachineHandle, MachineSpec, NetworkPolicy,
    OperationId, OperationKind, OperationRequest, OperationState, OutputLimit, PathRoot, ReadFile,
    RelPath, ResourceLimits, SandboxBackend, SandboxError, Sha256, StopProof, WorkspaceId,
};

fn ws() -> WorkspaceId {
    WorkspaceId::new("ws-1")
}

fn spec(generation: u64) -> MachineSpec {
    MachineSpec {
        workspace_id: ws(),
        generation: Generation(generation),
        image_digest: "sha256:image".into(),
        limits: ResourceLimits::DEFAULT,
        network: NetworkPolicy::default(),
    }
}

fn exec_limited(
    id: &str,
    m: &MachineHandle,
    command: &str,
    limit: OutputLimit,
) -> OperationRequest {
    OperationRequest {
        id: OperationId::new(id),
        workspace_id: m.workspace_id.clone(),
        generation: m.generation,
        args_sha256: Sha256::of(command.as_bytes()),
        kind: OperationKind::Exec {
            command: command.into(),
            cwd: RelPath::root(),
            timeout_ms: 1_000,
            output_limit: limit,
        },
    }
}

fn exec(id: &str, m: &MachineHandle, command: &str) -> OperationRequest {
    exec_limited(id, m, command, OutputLimit::LOG)
}

fn write(
    id: &str,
    m: &MachineHandle,
    path: &str,
    bytes: &[u8],
    mode: FileMode,
) -> OperationRequest {
    OperationRequest {
        id: OperationId::new(id),
        workspace_id: m.workspace_id.clone(),
        generation: m.generation,
        args_sha256: Sha256::of(bytes),
        kind: OperationKind::WriteFile {
            root: PathRoot::Workspace,
            path: RelPath::parse(path).unwrap(),
            sha256: Sha256::of(bytes),
            bytes: ByteSource::Inline(bytes.to_vec()),
            mode,
        },
    }
}

async fn started() -> (FakeSandboxBackend, MachineHandle) {
    let fake = FakeSandboxBackend::new();
    fake.script("sleep", Program::hang())
        .script("hi", Program::exit(0).stdout("hi"));
    let m = fake.ensure_machine(&spec(1)).await.unwrap();
    (fake, m)
}

async fn state(fake: &FakeSandboxBackend, m: &MachineHandle, id: &str) -> OperationState {
    let seen = fake.observe(m, &OperationId::new(id), 0).await.unwrap();
    seen.state
}

fn is_backend<T: std::fmt::Debug>(outcome: &Result<T, SandboxError>) -> bool {
    matches!(outcome, Err(SandboxError::Backend { .. }))
}

#[tokio::test]
async fn lost_ack_starts_the_operation_and_a_replay_finds_it() {
    let (fake, m) = started().await;
    fake.lose_next_ack();
    let op = exec("op-1", &m, "hi");
    assert!(is_backend(&fake.dispatch(&m, &op).await));
    assert_eq!(
        state(&fake, &m, "op-1").await,
        OperationState::Succeeded { exit: 0 }
    );
    let replay = fake.dispatch(&m, &op).await.unwrap();
    assert!(replay.existing);
    assert_eq!(fake.dispatch_calls(&op.id), 2);
    // A replay consumes no fault and the fault fired once.
    let next = fake.dispatch(&m, &exec("op-2", &m, "hi")).await.unwrap();
    assert!(!next.existing);
}

#[tokio::test]
async fn hung_dispatch_never_returns_but_started() {
    let (fake, m) = started().await;
    fake.hang_next_dispatch();
    let op = exec("op-1", &m, "sleep");
    let waited = tokio::time::timeout(Duration::from_millis(50), fake.dispatch(&m, &op)).await;
    assert!(waited.is_err(), "dispatch hangs");
    let id = op.id.clone();
    assert_eq!(
        fake.operation_state(&ws(), Generation(1), &id),
        Some(OperationState::Running)
    );
    assert_eq!(fake.dispatch_calls(&id), 1);
}

#[tokio::test]
async fn panicking_dispatch_started_the_operation_and_leaves_the_fake_usable() {
    let (fake, m) = started().await;
    fake.panic_next_dispatch();
    let (task_fake, task_m) = (fake.clone(), m.clone());
    let op = exec("op-1", &m, "hi");
    let task_op = op.clone();
    let joined = tokio::spawn(async move { task_fake.dispatch(&task_m, &task_op).await }).await;
    assert!(joined.unwrap_err().is_panic());
    assert_eq!(
        state(&fake, &m, "op-1").await,
        OperationState::Succeeded { exit: 0 }
    );
    assert!(fake.dispatch(&m, &op).await.unwrap().existing);
}

#[tokio::test]
async fn machine_dying_mid_run_answers_unreachable_until_stopped() {
    let (fake, m) = started().await;
    fake.die_after_next_dispatch();
    let ack = fake.dispatch(&m, &exec("op-1", &m, "sleep")).await.unwrap();
    assert_eq!(ack.state, OperationState::Running);
    assert!(is_backend(
        &fake.observe(&m, &OperationId::new("op-1"), 0).await
    ));
    assert!(is_backend(
        &fake.cancel(&m, &OperationId::new("op-1")).await
    ));
    let read = ReadFile {
        root: PathRoot::Workspace,
        raw: false,
        path: RelPath::parse("a").unwrap(),
        offset: 0,
        limit_bytes: 1,
        max_file_bytes: 1,
    };
    assert!(is_backend(&fake.read_file(&m, &read).await));
    assert!(is_backend(&fake.drain_denials(&m, 0).await));
    assert!(is_backend(&fake.ensure_machine(&spec(1)).await));
    assert_eq!(
        fake.stop_machine(&ws(), Generation(1)).await,
        Ok(StopProof::Stopped)
    );
    assert!(fake.ensure_machine(&spec(2)).await.is_ok());
    assert_eq!(fake.live_machines(), vec![(ws(), Generation(2))]);
}

#[tokio::test]
async fn make_unreachable_kills_the_live_machine_now() {
    let (fake, m) = started().await;
    fake.make_unreachable(&ws());
    let policy = NetworkPolicy::default();
    assert!(is_backend(&fake.apply_network_policy(&m, &policy).await));
}

#[tokio::test]
async fn refused_stop_is_unproven_and_keeps_the_machine() {
    let (fake, _m) = started().await;
    fake.refuse_stop(true);
    let proof = fake.stop_machine(&ws(), Generation(1)).await.unwrap();
    assert!(matches!(proof, StopProof::Unproven { .. }), "{proof:?}");
    assert_eq!(fake.live_machines(), vec![(ws(), Generation(1))]);
    // The next generation needs the stop first.
    assert!(is_backend(&fake.ensure_machine(&spec(2)).await));
    fake.refuse_stop(false);
    assert_eq!(
        fake.stop_machine(&ws(), Generation(1)).await,
        Ok(StopProof::Stopped)
    );
    assert_eq!(fake.stop_calls(), 2);
    assert!(fake.live_machines().is_empty());
}

#[tokio::test]
async fn no_record_is_unproven() {
    let fake = FakeSandboxBackend::new();
    let proof = fake.stop_machine(&ws(), Generation(1)).await.unwrap();
    assert!(matches!(proof, StopProof::Unproven { .. }), "{proof:?}");
    fake.ensure_machine(&spec(1)).await.unwrap();
    let skipped = fake.stop_machine(&ws(), Generation(2)).await.unwrap();
    assert!(matches!(skipped, StopProof::Unproven { .. }), "{skipped:?}");
}

#[tokio::test]
async fn served_generation_makes_older_handles_stale() {
    let (fake, m) = started().await;
    fake.serve_generation(&ws(), Generation(3));
    assert_eq!(fake.served_generation(&ws()), Some(Generation(3)));
    let stale = fake.dispatch(&m, &exec("op-1", &m, "hi")).await;
    assert_eq!(
        stale,
        Err(SandboxError::StaleGeneration {
            requested: Generation(1),
            current: Generation(3)
        })
    );
    assert!(fake.ensure_machine(&spec(2)).await.is_err());
}

#[tokio::test]
async fn endless_output_holds_no_more_than_the_limit() {
    let (fake, m) = started().await;
    fake.endless_next_exec();
    let limit = OutputLimit {
        max_bytes: 1024 * 1024,
        max_lines: None,
    };
    let op = exec_limited("op-1", &m, "hi", limit);
    fake.dispatch(&m, &op).await.unwrap();
    assert_eq!(state(&fake, &m, "op-1").await, OperationState::OutputLimit);
    assert_eq!(
        fake.output_held(&ws(), Generation(1), &op.id),
        limit.max_bytes
    );
    let lines = OutputLimit {
        max_bytes: 1024,
        max_lines: Some(3),
    };
    fake.script("yes", Program::endless("y\n"));
    let by_lines = exec_limited("op-2", &m, "yes", lines);
    fake.dispatch(&m, &by_lines).await.unwrap();
    let seen = fake.observe(&m, &by_lines.id, 0).await.unwrap();
    assert_eq!(
        (seen.state, seen.output.as_slice()),
        (OperationState::OutputLimit, &b"y\ny\ny\n"[..])
    );
    assert!(seen.truncated && seen.eof);
    // The fake's sequence is the byte offset into the held output.
    assert_eq!(seen.next_seq, 6);
    let tail = fake.observe(&m, &by_lines.id, 2).await.unwrap();
    assert_eq!(
        (tail.output.as_slice(), tail.next_seq),
        (
            &b"y
y
"[..],
            6
        )
    );
}

#[tokio::test]
async fn full_disk_fails_commands_and_writes_and_reports_exhausted() {
    let (fake, m) = started().await;
    fake.dispatch(&m, &exec("op-1", &m, "sleep")).await.unwrap();
    fake.fill_disk(&ws());
    let full = OperationState::Failed {
        exit: Some(DISK_FULL_EXIT),
        signal: None,
    };
    assert_eq!(state(&fake, &m, "op-1").await, full);
    assert!(fake.storage_usage(&ws()).await.unwrap().is_exhausted());
    fake.dispatch(&m, &write("op-2", &m, "a", b"a", FileMode::REGULAR))
        .await
        .unwrap();
    assert_eq!(state(&fake, &m, "op-2").await, full);
    fake.set_descriptor(BackendDescriptor {
        kind: "fake".into(),
        supports_status_after_host_loss: false,
        enforces_disk_quota: true,
    });
    let refused = fake.dispatch(&m, &exec("op-3", &m, "hi")).await;
    assert!(
        matches!(refused, Err(SandboxError::QuotaExceeded { .. })),
        "{refused:?}"
    );
    fake.stop_machine(&ws(), Generation(1)).await.unwrap();
    fake.destroy_storage(&ws()).await.unwrap();
    assert!(!fake.storage_usage(&ws()).await.unwrap().is_exhausted());
}

#[tokio::test]
async fn quota_comes_from_the_machine_limits() {
    let fake = FakeSandboxBackend::new();
    let tiny = MachineSpec {
        limits: ResourceLimits {
            disk_gib: 0,
            ..ResourceLimits::DEFAULT
        },
        ..spec(1)
    };
    let m = fake.ensure_machine(&tiny).await.unwrap();
    assert_eq!(fake.storage_usage(&ws()).await.unwrap().quota_bytes, 0);
    fake.dispatch(&m, &write("op-1", &m, "a", b"a", FileMode::REGULAR))
        .await
        .unwrap();
    let state = state(&fake, &m, "op-1").await;
    assert_eq!(state.exit(), Some(DISK_FULL_EXIT));
    assert_eq!(fake.volume_file(&ws(), "a"), None);
}

#[tokio::test]
async fn ensure_refuses_a_new_generation_while_the_old_runs() {
    let (fake, _m) = started().await;
    assert!(is_backend(&fake.ensure_machine(&spec(2)).await));
    assert_eq!(fake.machines_started(), 1);
}

#[tokio::test]
async fn counters_scripts_and_background() {
    let (fake, m) = started().await;
    fake.script("bg", Program::exit(0).leave_background());
    fake.dispatch(&m, &exec("op-1", &m, "bg")).await.unwrap();
    assert_eq!(
        state(&fake, &m, "op-1").await,
        OperationState::Succeeded { exit: 0 }
    );
    assert_eq!(fake.background_ended(), 1);
    fake.dispatch(&m, &exec("op-2", &m, "rm -rf /"))
        .await
        .unwrap();
    let unscripted = state(&fake, &m, "op-2").await;
    assert_eq!(unscripted.exit(), Some(UNSCRIPTED_EXIT));
    fake.script("sig", Program::killed_by(9));
    fake.dispatch(&m, &exec("op-3", &m, "sig")).await.unwrap();
    let killed = state(&fake, &m, "op-3").await;
    assert_eq!(
        killed,
        OperationState::Failed {
            exit: None,
            signal: Some(9)
        }
    );
    assert_eq!(fake.dispatch_calls_total(), 3);
    fake.dispatch(
        &m,
        &write("op-4", &m, "run.sh", b"#!/bin/sh\n", FileMode::EXECUTABLE),
    )
    .await
    .unwrap();
    state(&fake, &m, "op-4").await;
    assert_eq!(
        fake.volume_file_mode(&ws(), "run.sh"),
        Some(FileMode::EXECUTABLE)
    );
}

#[tokio::test]
async fn shared_clock_drives_run_time_and_deadline() {
    let clock = ManualClock::new(1_000);
    let fake = FakeSandboxBackend::with_clock(clock.clone());
    fake.script("slow", Program::exit(0).runs_for(500))
        .script("slower", Program::exit(0).runs_for(5_000));
    let m = fake.ensure_machine(&spec(1)).await.unwrap();
    fake.dispatch(&m, &exec("op-1", &m, "slow")).await.unwrap();
    fake.dispatch(&m, &exec("op-2", &m, "slower"))
        .await
        .unwrap();
    clock.advance(499);
    assert_eq!(state(&fake, &m, "op-1").await, OperationState::Running);
    clock.advance(1);
    assert_eq!(
        state(&fake, &m, "op-1").await,
        OperationState::Succeeded { exit: 0 }
    );
    fake.advance(500);
    // The 1 s deadline passed before the 5 s run time.
    assert_eq!(state(&fake, &m, "op-2").await, OperationState::TimedOut);
}

#[tokio::test]
async fn denied_connection_fails_with_the_refused_exit() {
    let (fake, m) = started().await;
    fake.script("curl", Program::connect("example.com", 443));
    fake.dispatch(&m, &exec("op-1", &m, "curl")).await.unwrap();
    assert_eq!(
        state(&fake, &m, "op-1").await.exit(),
        Some(CONNECT_REFUSED_EXIT)
    );
    let batch = fake.drain_denials(&m, 0).await.unwrap();
    assert_eq!(
        (batch.denials.len(), batch.cursor, batch.denials[0].at),
        (1, 1, 0)
    );
}

#[test]
fn clones_share_state() {
    let fake = FakeSandboxBackend::new();
    let other = fake.clone();
    other.advance(7);
    assert!(Arc::ptr_eq(&fake.clock(), &other.clock()));
    assert_eq!(fake.dispatch_calls_total(), 0);
}

#[tokio::test]
async fn stop_and_usage_let_finished_programs_land_first() {
    let (fake, m) = started().await;
    let bytes = b"landed";
    fake.lose_next_ack();
    let lost = fake
        .dispatch(&m, &write("op-1", &m, "a", bytes, FileMode::REGULAR))
        .await;
    assert!(is_backend(&lost));
    assert_eq!(
        fake.volume_file(&ws(), "a"),
        None,
        "inspection does not step"
    );
    assert_eq!(fake.storage_usage(&ws()).await.unwrap().used_bytes, 6);
    fake.dispatch(&m, &write("op-2", &m, "b", bytes, FileMode::REGULAR))
        .await
        .unwrap();
    assert_eq!(
        fake.stop_machine(&ws(), Generation(1)).await,
        Ok(StopProof::Stopped)
    );
    assert_eq!(fake.volume_file(&ws(), "b"), Some(bytes.to_vec()));
}
