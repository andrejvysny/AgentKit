//! The desktop quit (F-13), a delivery that arrives while an earlier one
//! still runs the operation (F-6), and what a delivered-again id reads
//! back: the fence as it is now, and a conflict for other arguments.

mod support;

use std::sync::Arc;
use std::sync::atomic::Ordering::SeqCst;

use agentkit_workspaces::{
    AgentId, FileRef, OperationId, OperationOutcome, OperationPhase, OperationState, RelPath,
    SandboxError, Settlement, Sha256, WorkspaceRepository, WorkspaceService,
};
use support::hooks::Hooked;
use support::{LONG, SLEEP, SUCCEEDED, TRUE, World, ctx, exec, home, settled};

type Task = tokio::task::JoinHandle<Result<OperationOutcome, SandboxError>>;

fn spawn(svc: &Arc<WorkspaceService>, op: &str, command: &str) -> Task {
    let (svc, context, command) = (svc.clone(), ctx(op, "a1"), command.to_owned());
    tokio::spawn(async move { svc.exec(&context, &home("a1"), exec(&command)).await })
}

async fn until_running(world: &World, op: &str) {
    for _ in 0..100_000 {
        let row = world.repo.operation(&OperationId::new(op)).await.unwrap();
        if row.is_some_and(|row| row.phase == OperationPhase::Running) {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("{op} never ran");
}

/// F-13: the quit closes admission, cancels the running command and
/// records it interrupted with the receipt read inside the grace, then
/// stops every machine.
#[tokio::test]
async fn quit_interrupts_the_running_command_and_stops_every_machine() {
    let world = World::new();
    let (svc, _) = world.service();
    svc.exec(&ctx("op-0", "a2"), &home("a2"), exec(TRUE))
        .await
        .unwrap();
    let running = spawn(&svc, "op-1", SLEEP);
    until_running(&world, "op-1").await;
    let report = svc.quiesce().await.unwrap();
    let done = running.await.unwrap().unwrap();
    let cancelled = OperationState::Cancelled;
    assert_eq!(done.settlement, Settlement::Interrupted(cancelled));
    let row = world.repo.operation(&OperationId::new("op-1")).await;
    let phase = row.unwrap().map(|row| row.phase);
    assert_eq!(phase, Some(OperationPhase::Interrupted(cancelled)));
    assert_eq!(report.stopped.len(), 2, "{report:?}");
    assert!(report.fenced.is_empty());
    assert!(world.fake.live_machines().is_empty());
    let refused = svc.exec(&ctx("op-2", "a1"), &home("a1"), exec(TRUE)).await;
    assert_eq!(refused.unwrap_err().code(), "sandbox_unavailable");
    let root = FileRef::workspace(RelPath::root());
    let read = svc.list(&home("a1"), &AgentId::new("a1"), &root, 1).await;
    assert_eq!(read.unwrap_err().code(), "sandbox_unavailable");
    assert_eq!(world.fake.dispatch_calls_total(), 2);
}

/// F-13 without a receipt: the backend stops answering, so the quit's
/// cancel gets no receipt; the command is uncertain, never interrupted.
#[tokio::test]
async fn quit_without_a_receipt_is_uncertain() {
    let world = World::new();
    let hooked = Hooked::new(world.backend.clone());
    let svc = world.service_with(hooked.clone());
    hooked.hang_after_dispatch.store(true, SeqCst);
    let running = spawn(&svc, "op-1", SLEEP);
    until_running(&world, "op-1").await;
    svc.quiesce().await.unwrap();
    let done = running.await.unwrap().unwrap();
    hooked.stop_hanging();
    assert!(done.settlement.is_uncertain(), "{:?}", done.settlement);
    let row = world.repo.operation(&OperationId::new("op-1")).await;
    let phase = row.unwrap().map(|row| row.phase);
    assert_eq!(phase, Some(OperationPhase::Uncertain));
}

/// F-6: a delivery of an id whose earlier delivery still runs it waits for
/// that one and reads its receipt back, past the lease wait, instead of
/// answering `sandbox_busy`. Dispatched once.
#[tokio::test]
async fn a_redelivery_waits_for_the_running_delivery() {
    let world = World::new();
    let (svc, _) = world.service();
    let first = spawn(&svc, "op-1", LONG);
    until_running(&world, "op-1").await;
    let again = spawn(&svc, "op-1", LONG);
    let first = first.await.unwrap().unwrap();
    let again = again.await.unwrap().unwrap();
    assert_eq!(settled(&first), SUCCEEDED);
    assert_eq!((settled(&again), again.replayed), (SUCCEEDED, true));
    assert_eq!(world.fake.dispatch_calls(&OperationId::new("op-1")), 1);
}

/// An uncertain row read back says whether the workspace is fenced now.
#[tokio::test]
async fn a_replayed_uncertain_row_reports_the_fence() {
    let world = World::new();
    let (svc, _) = world.service();
    world.fake.refuse_stop(true).lose_next_ack();
    let lost = svc
        .exec(&ctx("op-1", "a1"), &home("a1"), exec(TRUE))
        .await
        .unwrap();
    assert!(matches!(
        lost.settlement,
        Settlement::Uncertain { fenced: true, .. }
    ));
    let replayed = svc
        .exec(&ctx("op-1", "a1"), &home("a1"), exec(TRUE))
        .await
        .unwrap();
    assert!(replayed.replayed);
    assert!(
        matches!(
            replayed.settlement,
            Settlement::Uncertain { fenced: true, .. }
        ),
        "{:?}",
        replayed.settlement
    );
}

/// The argument hash is part of an operation's identity: the same id with
/// other arguments is a conflict, never a replay.
#[tokio::test]
async fn the_same_id_with_other_arguments_conflicts() {
    let world = World::new();
    let (svc, _) = world.service();
    svc.exec(&ctx("op-1", "a1"), &home("a1"), exec(TRUE))
        .await
        .unwrap();
    let mut other = ctx("op-1", "a1");
    other.args_sha256 = Sha256::of(b"other arguments");
    let refused = svc.exec(&other, &home("a1"), exec(TRUE)).await;
    assert!(
        matches!(refused, Err(SandboxError::OperationConflict(_))),
        "{refused:?}"
    );
    assert_eq!(world.fake.dispatch_calls(&OperationId::new("op-1")), 1);
}

/// A delivery waiting for the lease while the first one writes the row:
/// when the row it then finds has other arguments, it is a conflict, never
/// a read-back of the other call's receipt.
#[tokio::test]
async fn a_waiting_delivery_with_other_arguments_conflicts() {
    let world = World::new();
    let hooked = Hooked::new(world.backend.clone());
    let (svc, pause) = world.service_paused(hooked.clone());
    pause.freeze();
    hooked.gate_ensure.store(1, SeqCst);
    let first = spawn(&svc, "op-1", TRUE);
    hooked.entered.notified().await;
    let second = {
        let svc = svc.clone();
        let mut other = ctx("op-1", "a1");
        other.args_sha256 = Sha256::of(b"other arguments");
        tokio::spawn(async move { svc.exec(&other, &home("a1"), exec(TRUE)).await })
    };
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    hooked.release.notify_one();
    let first = first.await.unwrap().unwrap();
    pause.thaw();
    let second = second.await.unwrap();
    assert_eq!(settled(&first), SUCCEEDED);
    assert!(
        matches!(second, Err(SandboxError::OperationConflict(_))),
        "{second:?}"
    );
    assert_eq!(world.fake.dispatch_calls(&OperationId::new("op-1")), 1);
}
