//! Regression tests for the review probes of the workspace service: each
//! one reproduced a defect before its fix.

mod support;

use std::sync::atomic::Ordering::SeqCst;

use agentkit_workspaces::{
    AgentId, FileRef, Generation, GrantSource, HostPort, LeaseAcquire, OperationId, OperationPhase,
    RelPath, Settlement, StopProof, WorkspaceRepository, WorkspaceState,
};
use support::hooks::Hooked;
use support::repo::StallingRepo;
use support::{IMAGE, LONG, SLEEP, SUCCEEDED, TRUE, World, ctx, dead_holder, exec, home, settled};

async fn phase(world: &World, op: &str) -> Option<OperationPhase> {
    let row = world.repo.operation(&OperationId::new(op)).await.unwrap();
    row.map(|row| row.phase)
}

/// P1: the backend goes silent after the ack. Observe, stop and drain each
/// time out, which used to outlive the lease: the call answered an error
/// after dispatch and the unproven stop left the workspace busy. The call
/// must answer uncertain, and the workspace must be fenced.
#[tokio::test]
async fn a_silent_backend_after_dispatch_is_uncertain_and_fenced() {
    let world = World::new();
    let hooked = Hooked::new(world.backend.clone());
    let svc = world.service_with(hooked.clone());
    hooked.hang_after_dispatch.store(true, SeqCst);
    let done = svc
        .exec(&ctx("op-1", "a1"), &home("a1"), exec(SLEEP))
        .await
        .expect("an operation that may have run answers an outcome, never an error");
    hooked.stop_hanging();
    match &done.settlement {
        Settlement::Uncertain { fenced, .. } => assert!(*fenced, "{:?}", done.settlement),
        other => panic!("expected uncertain, got {other:?}"),
    }
    assert_eq!(phase(&world, "op-1").await, Some(OperationPhase::Uncertain));
    let ws = world.workspace(&home("a1")).await;
    assert!(
        matches!(ws.state, WorkspaceState::Fenced { .. }),
        "{:?}",
        ws.state
    );
    assert_eq!(world.fake.dispatch_calls(&OperationId::new("op-1")), 1);
}

/// P2: a crash right after recover advanced the generation, before it
/// unfenced, used to leave the workspace fenced at a generation that never
/// ran, for good. Recover must prove the generation the fence is about and
/// move past both.
#[tokio::test]
async fn recover_progresses_after_a_crash_between_advance_and_unfence() {
    let world = World::new();
    let (svc, _) = world.service();
    let binding = home("a1");
    svc.exec(&ctx("op-0", "a1"), &binding, exec(TRUE))
        .await
        .unwrap();
    world.fake.refuse_stop(true).lose_next_ack();
    let lost = svc
        .exec(&ctx("op-1", "a1"), &binding, exec(TRUE))
        .await
        .unwrap();
    assert!(lost.settlement.is_uncertain());
    let ws = world.workspace(&binding).await;
    assert!(matches!(ws.state, WorkspaceState::Fenced { .. }));
    world.fake.refuse_stop(false);
    // What the old two-step recover did up to its crash point.
    let proof = world.backend.fake.clone();
    assert_eq!(
        agentkit_workspaces::SandboxBackend::stop_machine(&proof, &ws.id, ws.generation)
            .await
            .unwrap(),
        StopProof::Stopped
    );
    let taken = world
        .repo
        .acquire_lease(&ws.id, &dead_holder("x"), "t", world.now(), 60_000)
        .await
        .unwrap();
    let LeaseAcquire::Acquired(lease) = taken else {
        panic!("lease is free")
    };
    world
        .repo
        .advance_generation(&lease, ws.generation, IMAGE, world.now())
        .await
        .unwrap();
    let (svc, _) = world.service();
    svc.recover_all().await.unwrap();
    svc.recover(&ws.id).await.expect("recover makes progress");
    let ws = world.workspace(&binding).await;
    assert_eq!(ws.state, WorkspaceState::Ready);
    let done = svc
        .exec(&ctx("op-9", "a1"), &binding, exec(TRUE))
        .await
        .unwrap();
    assert_eq!(settled(&done), SUCCEEDED);
    assert!(world.generations_proven().await.is_empty());
}

/// P4: a second service instance (another host on the same store) runs
/// startup recovery while the first one watches a live command. It must
/// not break the live lease or touch the operation.
#[tokio::test]
async fn recovery_spares_a_live_lease_of_another_instance() {
    let world = World::new();
    let (a, _) = world.service();
    let task = {
        let a = a.clone();
        tokio::spawn(async move { a.exec(&ctx("op-1", "a1"), &home("a1"), exec(LONG)).await })
    };
    for _ in 0..100_000 {
        if phase(&world, "op-1").await == Some(OperationPhase::Running) {
            break;
        }
        tokio::task::yield_now().await;
    }
    let other = world.service_on_host("host-b");
    let report = other.recover_all().await.unwrap();
    assert_eq!(report.leases_broken, 0);
    assert!(report.uncertain.is_empty(), "{report:?}");
    let done = task.await.unwrap().unwrap();
    assert_eq!(settled(&done), SUCCEEDED);
}

/// P5: an unleased read took a snapshot of generation 2, a stop proved 2
/// (never started) stopped and advanced to 3, then the read started 2. A
/// backend that does not enforce its proofs must not leave generation 2
/// running behind the record: the read drops and stops what it started.
#[tokio::test]
async fn a_read_never_leaves_a_superseded_generation_running() {
    let world = World::new();
    let hooked = Hooked::new(world.backend.clone());
    hooked.absent_proves.store(true, SeqCst);
    let svc = world.service_with(hooked.clone());
    let binding = home("a1");
    svc.exec(&ctx("op-0", "a1"), &binding, exec(TRUE))
        .await
        .unwrap();
    let ws = world.workspace(&binding).await;
    svc.stop(&ws.id).await.unwrap();
    hooked.gate_ensure.store(2, SeqCst);
    let reader = {
        let (svc, binding) = (svc.clone(), binding.clone());
        tokio::spawn(async move {
            let root = FileRef::workspace(RelPath::root());
            svc.list(&binding, &AgentId::new("a1"), &root, 1).await
        })
    };
    hooked.entered.notified().await;
    svc.stop(&ws.id).await.unwrap();
    hooked.release.notify_one();
    let _ = reader.await.unwrap();
    let ws = world.workspace(&binding).await;
    let behind: Vec<_> = world
        .fake
        .live_machines()
        .into_iter()
        .filter(|(_, generation)| *generation < ws.generation)
        .collect();
    assert!(
        behind.is_empty(),
        "{behind:?} still runs at {:?}",
        ws.generation
    );
    let next = svc
        .exec(&ctx("op-1", "a1"), &binding, exec(TRUE))
        .await
        .unwrap();
    assert_eq!(settled(&next), SUCCEEDED);
}

/// P7: one failed `destroy_storage` used to fence the workspace at a
/// generation that never ran, for good. A second delete must finish it.
#[tokio::test]
async fn delete_finishes_after_a_failed_destroy() {
    let world = World::new();
    let hooked = Hooked::new(world.backend.clone());
    let svc = world.service_with(hooked.clone());
    let binding = home("a1");
    svc.exec(&ctx("op-0", "a1"), &binding, exec(TRUE))
        .await
        .unwrap();
    let ws = world.workspace(&binding).await;
    hooked.fail_destroy.store(true, SeqCst);
    assert!(svc.delete(&ws.id).await.is_err());
    svc.delete(&ws.id)
        .await
        .expect("the second delete finishes");
    let stored = world.repo.workspace(&ws.id).await.unwrap().unwrap();
    assert_eq!(stored.state, WorkspaceState::Deleted);
    assert!(world.fake.live_machines().is_empty());
}

/// P7: after a failed destroy, recover lifts the fence and the workspace
/// takes commands again.
#[tokio::test]
async fn recover_lifts_the_fence_of_a_failed_destroy() {
    let world = World::new();
    let hooked = Hooked::new(world.backend.clone());
    let svc = world.service_with(hooked.clone());
    let binding = home("a1");
    svc.exec(&ctx("op-0", "a1"), &binding, exec(TRUE))
        .await
        .unwrap();
    let ws = world.workspace(&binding).await;
    hooked.fail_destroy.store(true, SeqCst);
    assert!(svc.delete(&ws.id).await.is_err());
    svc.recover(&ws.id).await.expect("recover");
    let stored = world.workspace(&binding).await;
    assert_eq!(stored.state, WorkspaceState::Ready);
    assert!(stored.generation > Generation(1));
    let done = svc
        .exec(&ctx("op-1", "a1"), &binding, exec(TRUE))
        .await
        .unwrap();
    assert_eq!(settled(&done), SUCCEEDED);
}

/// P5 on a backend that enforces its proofs: the read's late start of the
/// proven generation is refused, the read restarts on the current one, and
/// no generation at or below a proven one ever starts (INV-5).
#[tokio::test]
async fn a_proven_generation_is_never_started_by_a_late_read() {
    let world = World::new();
    world.fake.prove_absence(true);
    let hooked = Hooked::new(world.backend.clone());
    let svc = world.service_with(hooked.clone());
    let binding = home("a1");
    svc.exec(&ctx("op-0", "a1"), &binding, exec(TRUE))
        .await
        .unwrap();
    let ws = world.workspace(&binding).await;
    svc.stop(&ws.id).await.unwrap();
    hooked.gate_ensure.store(2, SeqCst);
    let reader = {
        let (svc, binding) = (svc.clone(), binding.clone());
        tokio::spawn(async move {
            let root = FileRef::workspace(RelPath::root());
            svc.list(&binding, &AgentId::new("a1"), &root, 1).await
        })
    };
    hooked.entered.notified().await;
    svc.stop(&ws.id).await.unwrap();
    hooked.release.notify_one();
    reader
        .await
        .unwrap()
        .expect("the read restarts on the current generation");
    let current = world.workspace(&binding).await.generation;
    assert_eq!(world.fake.live_machines(), vec![(ws.id.clone(), current)]);
    assert!(world.generations_proven().await.is_empty());
}

/// P3: a policy push that read the grants before a revoke used to land
/// after the revoke's own push and undo it. Pushes are serialized and read
/// the grants inside the lock, so the machine ends with the revoke.
#[tokio::test]
async fn a_stale_policy_push_never_undoes_a_revoke() {
    let world = World::new();
    let hooked = Hooked::new(world.backend.clone());
    let (svc, pause) = world.service_paused(hooked.clone());
    let binding = home("a1");
    svc.exec(&ctx("op-0", "a1"), &binding, exec(TRUE))
        .await
        .unwrap();
    let ws = world.workspace(&binding).await;
    let target = HostPort::new("example.com", 443).unwrap();
    let grant = svc
        .grant(&ws.id, &target, GrantSource::Owner)
        .await
        .unwrap();
    hooked.gate_policy.store(true, SeqCst);
    // The gated push must outlast the revoke without timing out.
    pause.freeze();
    let call = {
        let (svc, binding) = (svc.clone(), binding.clone());
        tokio::spawn(async move { svc.exec(&ctx("op-1", "a1"), &binding, exec(TRUE)).await })
    };
    hooked.entered.notified().await;
    let revoke = {
        let svc = svc.clone();
        tokio::spawn(async move { svc.revoke(&grant.id).await })
    };
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    hooked.release.notify_one();
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    pause.thaw();
    revoke.await.unwrap().unwrap();
    call.await.unwrap().unwrap();
    let generation = world.workspace(&binding).await.generation;
    let policy = world.fake.machine_policy(&ws.id, generation).unwrap();
    assert!(!policy.allows(&target), "the revoked target is reachable");
}

/// P6: the worker stalls between writing the row and dispatching it, long
/// enough for its lease to run out. The renewal right before the dispatch
/// finds the lease lost: nothing is dispatched, the call answers
/// uncertain, and the next holder settles the row without dispatching it.
#[tokio::test]
async fn a_worker_whose_lease_ran_out_after_the_row_dispatches_nothing() {
    let world = World::new();
    let repo = StallingRepo::new(world.repo.clone(), world.clock.clone());
    let svc = world.service_over(repo.clone(), world.backend.clone(), world.config.clone());
    let binding = home("a1");
    svc.exec(&ctx("op-0", "a1"), &binding, exec(TRUE))
        .await
        .unwrap();
    repo.stall_after_insert_ms
        .store(world.config.lease_ttl_ms + 1_000, SeqCst);
    let stalled = svc
        .exec(&ctx("op-1", "a1"), &binding, exec(TRUE))
        .await
        .expect("a call whose row exists answers an outcome");
    assert!(
        stalled.settlement.is_uncertain(),
        "{:?}",
        stalled.settlement
    );
    let id = OperationId::new("op-1");
    assert_eq!(world.fake.dispatch_calls(&id), 0);
    let next = svc
        .exec(&ctx("op-2", "a1"), &binding, exec(TRUE))
        .await
        .unwrap();
    assert_eq!(settled(&next), SUCCEEDED);
    assert_eq!(phase(&world, "op-1").await, Some(OperationPhase::Uncertain));
    assert_eq!(world.fake.dispatch_calls(&id), 0);
}

/// Finding 10: machines left running across a restart, with no open
/// operation, are stopped by startup recovery. An unproven stop fences
/// only a workspace whose rows show a machine of that generation ran.
#[tokio::test]
async fn recovery_stops_machines_left_from_before_a_restart() {
    let world = World::new();
    let (svc, _) = world.service();
    svc.exec(&ctx("op-0", "a1"), &home("a1"), exec(TRUE))
        .await
        .unwrap();
    let unused = svc.resolve(&home("a2")).await.unwrap();
    let (fresh, _) = world.service();
    let report = fresh.recover_all().await.unwrap();
    let used = world.workspace(&home("a1")).await;
    assert_eq!(report.stopped, vec![used.id.clone()]);
    assert!(world.fake.live_machines().is_empty());
    assert_eq!(used.generation, Generation(2));
    assert_eq!(world.workspace(&home("a2")).await, unused);
    // The next generation runs; then its stop cannot be proven.
    fresh
        .exec(&ctx("op-1", "a1"), &home("a1"), exec(TRUE))
        .await
        .unwrap();
    world.fake.refuse_stop(true);
    let (third, _) = world.service();
    let report = third.recover_all().await.unwrap();
    assert_eq!(report.fenced, vec![used.id.clone()]);
    let unused_now = world.workspace(&home("a2")).await;
    assert_eq!(unused_now.state, WorkspaceState::Ready);
}
