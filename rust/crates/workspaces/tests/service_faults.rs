//! The failure matrix of the workspace service (spec section 19) on the
//! fake backend: F-1..F-7, F-15, and INV-1 and INV-3 directly (F-8 and
//! INV-6 are in `service_fence.rs`).
//! A host crash is an aborted task (its futures dropped mid-wait); a
//! restart is a new service over the same repository and backend.

mod support;

use std::sync::Arc;
use std::sync::atomic::Ordering::SeqCst;

use agentkit_workspaces::{
    AgentId, BackendDescriptor, FileRef, Generation, OperationId, OperationPhase, OperationState,
    SandboxError, Settlement, WorkspaceRepository, WorkspaceService, WorkspaceState,
};
use support::hooks::Hooked;
use support::{
    MARK, SLEEP, SLOW, SUCCEEDED, TRUE, World, assert_uncertain, ctx, exec, home, settled,
};

async fn phase(world: &World, op: &str) -> Option<OperationPhase> {
    let row = world.repo.operation(&OperationId::new(op)).await.unwrap();
    row.map(|row| row.phase)
}

/// Let spawned work run until `op` reaches `want`.
async fn until(world: &World, op: &str, want: OperationPhase) {
    for _ in 0..100_000 {
        if phase(world, op).await == Some(want) {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("{op} never reached {want:?}");
}

fn spawn_exec(
    svc: &Arc<WorkspaceService>,
    op: &str,
    command: &str,
) -> tokio::task::JoinHandle<Result<agentkit_workspaces::OperationOutcome, SandboxError>> {
    let (svc, ctx, command) = (svc.clone(), ctx(op, "a1"), command.to_owned());
    tokio::spawn(async move { svc.exec(&ctx, &home("a1"), exec(&command)).await })
}

async fn generation(world: &World) -> Generation {
    world.workspace(&home("a1")).await.generation
}

/// F-1: the host died after an admitted call took the lease, before its
/// row existed. Nothing was dispatched; recovery only breaks the lease; the
/// workspace is ready; delivered again, the admitted call runs once.
#[tokio::test]
async fn host_death_before_the_row_leaves_nothing_to_recover() {
    let world = World::new();
    let hooked = Hooked::new(world.backend.clone());
    let before = world.service_with(hooked.clone());
    let ws = before.resolve(&home("a1")).await.unwrap();
    hooked.gate_ensure.store(1, SeqCst);
    let admitted = {
        let before = before.clone();
        tokio::spawn(async move {
            before
                .exec(&ctx("op-1", "a1"), &home("a1"), exec(TRUE))
                .await
        })
    };
    hooked.entered.notified().await;
    let held = world.repo.lease(&ws.id, world.now()).await.unwrap();
    assert!(held.is_some(), "the admitted call holds the lease");
    admitted.abort();
    assert!(admitted.await.is_err(), "the host died mid-call");
    assert_eq!(phase(&world, "op-1").await, None, "no row was written");
    let (svc, _) = world.service();
    let report = svc.recover_all().await.unwrap();
    assert_eq!(report.leases_broken, 1);
    assert!(report.uncertain.is_empty() && report.settled.is_empty());
    assert_eq!(world.fake.dispatch_calls_total(), 0);
    assert_eq!(
        world.workspace(&home("a1")).await.state,
        WorkspaceState::Ready
    );
    let done = svc
        .exec(&ctx("op-1", "a1"), &home("a1"), exec(TRUE))
        .await
        .unwrap();
    assert_eq!(settled(&done), SUCCEEDED);
    assert_eq!(world.fake.dispatch_calls(&OperationId::new("op-1")), 1);
}

/// F-2: the ack is lost after the guest started the command. Uncertain,
/// machine stopped, dispatched once, generation bumps at the next use.
#[tokio::test]
async fn lost_ack_is_uncertain_and_the_generation_bumps_at_next_use() {
    let world = World::new();
    let (svc, _) = world.service();
    svc.exec(&ctx("op-0", "a1"), &home("a1"), exec(TRUE))
        .await
        .unwrap();
    world.fake.lose_next_ack();
    let lost = svc
        .exec(&ctx("op-1", "a1"), &home("a1"), exec(MARK))
        .await
        .unwrap();
    assert!(matches!(
        lost.settlement,
        Settlement::Uncertain { fenced: false, .. }
    ));
    assert_eq!(world.fake.dispatch_calls(&OperationId::new("op-1")), 1);
    assert!(
        world.fake.live_machines().is_empty(),
        "the machine was stopped"
    );
    assert_eq!(phase(&world, "op-1").await, Some(OperationPhase::Uncertain));
    let ws = world.workspace(&home("a1")).await;
    assert_eq!(
        (ws.state, ws.generation),
        (WorkspaceState::Busy, Generation(1))
    );
    // A read needs a machine: with the lease free it proves the stop,
    // advances, and reads from the fresh generation.
    let agent = AgentId::new("a1");
    let root = FileRef::workspace(agentkit_workspaces::RelPath::root());
    svc.list(&home("a1"), &agent, &root, 1).await.unwrap();
    assert_eq!(generation(&world).await, Generation(2));
    let next = svc
        .exec(&ctx("op-2", "a1"), &home("a1"), exec(TRUE))
        .await
        .unwrap();
    assert_eq!(
        (settled(&next), next.generation),
        (SUCCEEDED, Generation(2))
    );
    // Delivered again: read back, never run again.
    let again = svc
        .exec(&ctx("op-1", "a1"), &home("a1"), exec(MARK))
        .await
        .unwrap();
    assert!(again.replayed);
    assert_uncertain(&again);
    assert_eq!(world.fake.dispatch_calls(&OperationId::new("op-1")), 1);
    assert_eq!(world.generations_proven().await, Vec::<String>::new());
}

/// F-3: the host dies while the command runs. Recovery stops the machine
/// (no process of it survives) and marks the operation uncertain.
#[tokio::test]
async fn host_death_while_running_is_uncertain_after_recovery() {
    let world = World::new();
    let (svc, _) = world.service();
    let task = spawn_exec(&svc, "op-1", SLEEP);
    until(&world, "op-1", OperationPhase::Running).await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    let (svc, _) = world.service();
    let report = svc.recover_all().await.unwrap();
    assert_eq!(report.uncertain, vec![OperationId::new("op-1")]);
    assert_eq!(report.leases_broken, 1);
    assert!(world.fake.live_machines().is_empty());
    assert_eq!(world.fake.dispatch_calls(&OperationId::new("op-1")), 1);
    let next = svc
        .exec(&ctx("op-2", "a1"), &home("a1"), exec(TRUE))
        .await
        .unwrap();
    assert_eq!(next.generation, Generation(2));
    assert_eq!(world.generations_proven().await, Vec::<String>::new());
}

/// F-4: the command exited but the host died before storing the receipt.
/// Without status after host loss: uncertain. With it: the receipt
/// settles it.
#[tokio::test]
async fn exited_command_after_host_death_depends_on_status_support() {
    for status_after_loss in [false, true] {
        let world = World::new();
        world.fake.set_descriptor(BackendDescriptor {
            kind: "fake".into(),
            supports_status_after_host_loss: status_after_loss,
            enforces_disk_quota: false,
        });
        let (svc, _) = world.service();
        let task = spawn_exec(&svc, "op-1", SLOW);
        until(&world, "op-1", OperationPhase::Running).await;
        task.abort();
        let _ = task.await;
        world.clock.advance(5_000);
        let (svc, _) = world.service();
        let report = svc.recover_all().await.unwrap();
        let row = phase(&world, "op-1").await.unwrap();
        if status_after_loss {
            assert_eq!(report.settled, vec![OperationId::new("op-1")]);
            assert_eq!(row, OperationPhase::Terminal(SUCCEEDED));
            assert_eq!(
                world.workspace(&home("a1")).await.state,
                WorkspaceState::Ready
            );
        } else {
            assert_eq!(report.uncertain, vec![OperationId::new("op-1")]);
            assert_eq!(row, OperationPhase::Uncertain);
            assert!(world.fake.live_machines().is_empty());
        }
        assert_eq!(world.fake.dispatch_calls(&OperationId::new("op-1")), 1);
    }
}

/// F-5: worker A stalls past its lease while the command runs; worker B
/// takes the same operation, reconciles it (cancel, receipt) and never
/// dispatches it. A's later writes are rejected by the fence and it
/// reports the outcome as uncertain to its caller.
#[tokio::test]
async fn lease_lost_worker_writes_nothing_and_the_new_owner_reconciles() {
    let world = World::new();
    let (a, a_pause) = world.service();
    let (b, _) = world.service();
    let task = spawn_exec(&a, "op-x", SLEEP);
    until(&world, "op-x", OperationPhase::Running).await;
    a_pause.freeze();
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    world.clock.advance(world.config.lease_ttl_ms + 1_000);
    let taken = b
        .exec(&ctx("op-x", "a1"), &home("a1"), exec(SLEEP))
        .await
        .unwrap();
    assert_eq!(settled(&taken), OperationState::Cancelled);
    assert!(!taken.replayed);
    let receipt = phase(&world, "op-x").await;
    a_pause.thaw();
    let old = task.await.unwrap().unwrap();
    assert_uncertain(&old);
    assert!(format!("{:?}", old.settlement).contains("lease was lost"));
    assert_eq!(phase(&world, "op-x").await, receipt, "A wrote nothing");
    assert_eq!(world.fake.dispatch_calls(&OperationId::new("op-x")), 1);
    assert_eq!(
        world.workspace(&home("a1")).await.state,
        WorkspaceState::Ready
    );
}

/// F-6: the same call delivered twice is one operation, in sequence and
/// concurrently.
#[tokio::test]
async fn the_same_call_delivered_twice_runs_once() {
    let world = World::new();
    let (svc, _) = world.service();
    let first = svc
        .exec(&ctx("op-1", "a1"), &home("a1"), exec(MARK))
        .await
        .unwrap();
    let second = svc
        .exec(&ctx("op-1", "a1"), &home("a1"), exec(MARK))
        .await
        .unwrap();
    assert_eq!((settled(&first), settled(&second)), (SUCCEEDED, SUCCEEDED));
    assert!(!first.replayed && second.replayed);
    let ws = world.workspace(&home("a1")).await;
    assert_eq!(
        world.fake.volume_file(&ws.id, "mark.txt"),
        Some(b"x".to_vec())
    );
    let both = [
        spawn_exec(&svc, "op-2", SLOW),
        spawn_exec(&svc, "op-2", SLOW),
    ];
    for task in both {
        assert_eq!(settled(&task.await.unwrap().unwrap()), SUCCEEDED);
    }
    assert_eq!(world.fake.dispatch_calls(&OperationId::new("op-2")), 1);
    let other = svc.exec(&ctx("op-1", "a1"), &home("a1"), exec(TRUE)).await;
    assert_eq!(other.unwrap_err().code(), "sandbox_operation_conflict");
}

/// F-15, INV-7: a settled operation delivered again (after a restart)
/// answers its receipt; no dispatch and no machine start.
#[tokio::test]
async fn a_settled_operation_resubmitted_replays_its_receipt() {
    let world = World::new();
    let (svc, _) = world.service();
    svc.exec(&ctx("op-1", "a1"), &home("a1"), exec(MARK))
        .await
        .unwrap();
    let (dispatched, started) = (
        world.fake.dispatch_calls_total(),
        world.fake.machines_started(),
    );
    let (svc, _) = world.service();
    let replay = svc
        .exec(&ctx("op-1", "a1"), &home("a1"), exec(MARK))
        .await
        .unwrap();
    assert!(replay.replayed && replay.output.is_empty());
    assert_eq!(settled(&replay), SUCCEEDED);
    assert_eq!(world.fake.dispatch_calls_total(), dispatched);
    assert_eq!(world.fake.machines_started(), started);
}

/// INV-1: a dispatch that panics (the host dying inside it) leaves the
/// row written before it; after a restart it reads as uncertain.
#[tokio::test]
async fn a_panicking_dispatch_leaves_a_row_recovery_marks_uncertain() {
    let world = World::new();
    let (svc, _) = world.service();
    world.fake.panic_next_dispatch();
    let crashed = spawn_exec(&svc, "op-1", TRUE).await;
    assert!(crashed.unwrap_err().is_panic());
    assert_eq!(
        phase(&world, "op-1").await,
        Some(OperationPhase::Dispatching)
    );
    let (svc, _) = world.service();
    let report = svc.recover_all().await.unwrap();
    assert_eq!(report.uncertain, vec![OperationId::new("op-1")]);
    assert_eq!(phase(&world, "op-1").await, Some(OperationPhase::Uncertain));
    assert_eq!(world.fake.dispatch_calls(&OperationId::new("op-1")), 1);
}

/// INV-3: a dispatch that never answers is uncertain after the call
/// timeout; a restart and a redelivery never dispatch it again.
#[tokio::test]
async fn a_hung_dispatch_is_never_dispatched_again() {
    let world = World::new();
    let (svc, _) = world.service();
    world.fake.hang_next_dispatch();
    let hung = svc
        .exec(&ctx("op-1", "a1"), &home("a1"), exec(SLEEP))
        .await
        .unwrap();
    // This worker itself records the unknown outcome and stops the machine.
    assert!(matches!(
        hung.settlement,
        Settlement::Uncertain { fenced: false, .. }
    ));
    assert!(!format!("{:?}", hung.settlement).contains("lease was lost"));
    assert_eq!(phase(&world, "op-1").await, Some(OperationPhase::Uncertain));
    assert!(world.fake.live_machines().is_empty());
    let (svc, _) = world.service();
    svc.recover_all().await.unwrap();
    let again = svc
        .exec(&ctx("op-1", "a1"), &home("a1"), exec(SLEEP))
        .await
        .unwrap();
    assert!(again.replayed);
    assert_eq!(world.fake.dispatch_calls(&OperationId::new("op-1")), 1);
    assert!(world.fake.live_machines().is_empty());
}

/// A machine that dies mid-run makes the operation uncertain; the stop is
/// still proven, so the next use gets a fresh generation.
#[tokio::test]
async fn a_machine_dying_mid_run_is_uncertain() {
    let world = World::new();
    let (svc, _) = world.service();
    world.fake.die_after_next_dispatch();
    let died = svc
        .exec(&ctx("op-1", "a1"), &home("a1"), exec(SLEEP))
        .await
        .unwrap();
    assert!(matches!(
        died.settlement,
        Settlement::Uncertain { fenced: false, .. }
    ));
    assert!(world.fake.live_machines().is_empty());
    let next = svc
        .exec(&ctx("op-2", "a1"), &home("a1"), exec(TRUE))
        .await
        .unwrap();
    assert_eq!(next.generation, Generation(2));
}

/// Cancelled before dispatch: nothing starts. Cancelled while running:
/// the guest ends it and the receipt says so.
#[tokio::test]
async fn cancellation_before_and_after_dispatch() {
    let world = World::new();
    let (svc, _) = world.service();
    let early = ctx("op-1", "a1");
    early.cancellation.cancel();
    let refused = svc.exec(&early, &home("a1"), exec(TRUE)).await;
    assert_eq!(refused.unwrap_err().code(), "sandbox_cancelled");
    assert_eq!(world.fake.dispatch_calls_total(), 0);
    assert_eq!(phase(&world, "op-1").await, None);
    let late = ctx("op-2", "a1");
    let token = late.cancellation.clone();
    let task = {
        let svc = svc.clone();
        tokio::spawn(async move { svc.exec(&late, &home("a1"), exec(SLEEP)).await })
    };
    until(&world, "op-2", OperationPhase::Running).await;
    token.cancel();
    let done = task.await.unwrap().unwrap();
    assert_eq!(settled(&done), OperationState::Cancelled);
}

/// An open operation whose worker vanished (no restart) is adopted by the
/// next call once its lease expired: cancelled, settled, never
/// dispatched again; then the new call runs.
#[tokio::test]
async fn an_orphaned_operation_is_adopted_by_the_next_call() {
    let world = World::new();
    let (svc, _) = world.service();
    let task = spawn_exec(&svc, "op-1", SLEEP);
    until(&world, "op-1", OperationPhase::Running).await;
    task.abort();
    let _ = task.await;
    world.clock.advance(world.config.lease_ttl_ms + 1);
    let next = svc
        .exec(&ctx("op-2", "a1"), &home("a1"), exec(TRUE))
        .await
        .unwrap();
    assert_eq!(settled(&next), SUCCEEDED);
    let cancelled = OperationPhase::Terminal(OperationState::Cancelled);
    assert_eq!(phase(&world, "op-1").await, Some(cancelled));
    assert_eq!(world.fake.dispatch_calls(&OperationId::new("op-1")), 1);
}
