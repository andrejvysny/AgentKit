//! F-8 and INV-6 on the fake backend: a stop that cannot be proven fences
//! the workspace; every call answers sandbox_fenced and nothing is
//! dispatched until an explicit recover proves the stop (INV-5).

mod support;

use std::sync::Arc;

use agentkit_workspaces::{
    AgentId, FileMode, FileRef, Generation, Settlement, StopOutcome, WorkspaceService,
    WorkspaceState, WriteRequest,
};
use support::{SUCCEEDED, TRUE, World, assert_fenced, ctx, exec, home, rel, settled};

async fn generation(world: &World) -> Generation {
    world.workspace(&home("a1")).await.generation
}

fn note() -> WriteRequest {
    WriteRequest {
        file: FileRef::workspace(rel("a.txt")),
        bytes: b"a".to_vec(),
        mode: FileMode::REGULAR,
    }
}

/// A workspace whose last operation's stop is unproven (F-8): fenced.
async fn fenced_world() -> (World, Arc<WorkspaceService>) {
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
    assert!(matches!(
        lost.settlement,
        Settlement::Uncertain { fenced: true, .. }
    ));
    let ws = world.workspace(&binding).await;
    assert!(matches!(ws.state, WorkspaceState::Fenced { .. }));
    (world, svc)
}

/// F-8, INV-6: on a fenced workspace every call answers sandbox_fenced and
/// nothing is dispatched; the status still answers, with the fence.
#[tokio::test]
async fn a_fenced_workspace_admits_nothing() {
    let (world, svc) = fenced_world().await;
    let (binding, agent) = (home("a1"), AgentId::new("a1"));
    let ws = world.workspace(&binding).await;
    let dispatched = world.fake.dispatch_calls_total();
    let file = FileRef::workspace(rel("a.txt"));
    assert_fenced(svc.exec(&ctx("op-2", "a1"), &binding, exec(TRUE)).await);
    assert_fenced(svc.write_file(&ctx("op-3", "a1"), &binding, note()).await);
    assert_fenced(
        svc.copy_in(&ctx("op-4", "a1"), &binding, "f", b"x".to_vec())
            .await,
    );
    assert_fenced(svc.read_file(&binding, &agent, &file, 0, 10).await);
    assert_fenced(svc.list(&binding, &agent, &file, 1).await);
    assert_fenced(svc.read_for_export(&binding, &agent, &file).await);
    assert_fenced(svc.reset_home(&ws.id).await);
    assert_fenced(svc.stop(&ws.id).await);
    let status = svc.status(&binding, &agent).await.unwrap();
    assert!(matches!(
        status.workspace.unwrap().state,
        WorkspaceState::Fenced { .. }
    ));
    // A replay reads the row; it admits nothing.
    let replay = svc
        .exec(&ctx("op-1", "a1"), &binding, exec(TRUE))
        .await
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(world.fake.dispatch_calls_total(), dispatched);
}

/// F-8, INV-5: recover retries the proof; the generation advances and the
/// fence lifts only once the stop is proven.
#[tokio::test]
async fn recover_lifts_the_fence_only_with_a_proof() {
    let (world, svc) = fenced_world().await;
    let ws = world.workspace(&home("a1")).await;
    assert!(matches!(
        svc.recover(&ws.id).await.unwrap(),
        StopOutcome::Fenced { .. }
    ));
    assert_eq!(generation(&world).await, Generation(1));
    world.fake.refuse_stop(false);
    let recovered = svc.recover(&ws.id).await.unwrap();
    assert_eq!(
        recovered,
        StopOutcome::Stopped {
            next_generation: Generation(2)
        }
    );
    let after = svc
        .exec(&ctx("op-5", "a1"), &home("a1"), exec(TRUE))
        .await
        .unwrap();
    assert_eq!(
        (settled(&after), after.generation),
        (SUCCEEDED, Generation(2))
    );
    assert_eq!(world.generations_proven().await, Vec::<String>::new());
}
