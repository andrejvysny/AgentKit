//! One writer per workspace (INV-4): F-9, F-10, F-17 and a sampled check
//! that no two mutating operations of one workspace are ever open, in the
//! projection or in the guest.

mod support;

use std::sync::Arc;

use agentkit_workspaces::{
    AgentId, Binding, FileMode, FileRef, OpContext, OperationOutcome, SandboxError, WorkspaceId,
    WorkspaceKey, WorkspaceKind, WorkspaceRepository, WorkspaceService, WriteRequest,
};
use support::{LONG, SLOW, SUCCEEDED, TRUE, World, chat, ctx, exec, rel, settled};
use tokio::task::JoinHandle;

type Task = JoinHandle<Result<OperationOutcome, SandboxError>>;

fn team() -> Binding {
    Binding {
        key: WorkspaceKey::new("shared:team"),
        kind: WorkspaceKind::Shared,
        owner_scope: "owner:1".into(),
        folder: None,
    }
}

async fn shared_world(agents: &[&str]) -> (World, Arc<WorkspaceService>, WorkspaceId) {
    let world = World::new();
    let (svc, _) = world.service();
    let ws = svc.resolve(&team()).await.unwrap();
    for agent in agents {
        svc.add_member(&ws.id, &AgentId::new(*agent)).await.unwrap();
    }
    (world, svc, ws.id)
}

fn spawn(svc: &Arc<WorkspaceService>, ctx: OpContext, binding: Binding, command: &str) -> Task {
    let (svc, command) = (svc.clone(), command.to_owned());
    tokio::spawn(async move { svc.exec(&ctx, &binding, exec(&command)).await })
}

/// Let spawned work run until `ws` has an open operation.
async fn until_busy(world: &World, ws: &WorkspaceId) {
    for _ in 0..100_000 {
        let open = world.repo.list_open_operations().await.unwrap();
        if open.iter().any(|op| &op.workspace_id == ws) {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("{ws} never got busy");
}

/// F-9: two agents at once on a shared workspace. The second waits; after
/// the limit it gets sandbox_busy naming the holder.
#[tokio::test]
async fn a_second_agent_waits_then_hears_who_holds_the_workspace() {
    let (world, svc, ws) = shared_world(&["a1", "a2"]).await;
    let first = spawn(&svc, ctx("op-1", "a1"), team(), LONG);
    until_busy(&world, &ws).await;
    let started = world.now();
    let busy = svc.exec(&ctx("op-2", "a2"), &team(), exec(TRUE)).await;
    match busy {
        Err(SandboxError::Busy { holder }) => {
            assert_eq!(
                (holder.agent_id.as_str(), holder.run_id.as_str()),
                ("a1", "run-a1")
            );
        }
        other => panic!("expected sandbox_busy, got {other:?}"),
    }
    let waited = world.now() - started;
    let limit = i64::try_from(world.config.lease_wait_ms).unwrap();
    assert!(waited >= limit, "waited {waited} ms");
    assert_eq!(settled(&first.await.unwrap().unwrap()), SUCCEEDED);
    let after = svc
        .exec(&ctx("op-3", "a2"), &team(), exec(TRUE))
        .await
        .unwrap();
    assert_eq!(settled(&after), SUCCEEDED);
}

/// A short holder: the second agent waits and then runs, after the first
/// ended.
#[tokio::test]
async fn a_second_agent_runs_after_a_short_holder() {
    let (world, svc, _) = shared_world(&["a1", "a2"]).await;
    let first = spawn(&svc, ctx("op-1", "a1"), team(), SLOW);
    let second = spawn(&svc, ctx("op-2", "a2"), team(), TRUE);
    let (a, b) = (
        first.await.unwrap().unwrap(),
        second.await.unwrap().unwrap(),
    );
    assert_eq!((settled(&a), settled(&b)), (SUCCEEDED, SUCCEEDED));
    assert!(b.started_at >= a.ended_at.unwrap(), "never two writers");
    assert_eq!(world.fake.dispatch_calls_total(), 2);
}

/// F-10: a routine and an interactive run of one agent overlap: same rule.
#[tokio::test]
async fn two_runs_of_one_agent_never_write_at_once() {
    let world = World::new();
    let (svc, _) = world.service();
    let binding = support::home("a1");
    let routine = OpContext {
        run_id: "routine".into(),
        ..ctx("op-1", "a1")
    };
    let first = spawn(&svc, routine, binding.clone(), LONG);
    let ws = svc.resolve(&binding).await.unwrap();
    until_busy(&world, &ws.id).await;
    let chat_run = OpContext {
        run_id: "chat".into(),
        ..ctx("op-2", "a1")
    };
    match svc.exec(&chat_run, &binding, exec(TRUE)).await {
        Err(SandboxError::Busy { holder }) => assert_eq!(holder.run_id, "routine"),
        other => panic!("expected sandbox_busy, got {other:?}"),
    }
    assert_eq!(settled(&first.await.unwrap().unwrap()), SUCCEEDED);
}

fn write_task(svc: &Arc<WorkspaceService>, op: &str, chat_id: &str) -> Task {
    let (svc, ctx, binding) = (svc.clone(), ctx(op, "a1"), chat("p1", chat_id));
    let request = WriteRequest {
        file: FileRef::workspace(rel("a.txt")),
        bytes: chat_id.as_bytes().to_vec(),
        mode: FileMode::REGULAR,
    };
    tokio::spawn(async move { svc.write_file(&ctx, &binding, request).await })
}

/// F-17: two chats of one project use the sandbox for the first time at
/// once: one workspace, a folder each, one writer at a time.
#[tokio::test]
async fn first_use_by_two_chats_creates_one_workspace() {
    let world = World::new();
    let (svc, _) = world.service();
    let slow = spawn(&svc, ctx("op-0", "a1"), chat("p1", "c0"), SLOW);
    let tasks = [
        write_task(&svc, "op-1", "c1"),
        write_task(&svc, "op-2", "c2"),
    ];
    assert_eq!(settled(&slow.await.unwrap().unwrap()), SUCCEEDED);
    for task in tasks {
        assert_eq!(settled(&task.await.unwrap().unwrap()), SUCCEEDED);
    }
    let all = world.repo.list_workspaces().await.unwrap();
    assert_eq!(all.len(), 1, "exactly one workspace for the project");
    let ws = &all[0].id;
    assert_eq!(
        world.fake.volume_file(ws, "chats/c1/a.txt"),
        Some(b"c1".to_vec())
    );
    assert_eq!(
        world.fake.volume_file(ws, "chats/c2/a.txt"),
        Some(b"c2".to_vec())
    );
}

/// INV-4, sampled: four agents race; at every step at most one operation
/// of the workspace is open in the projection and in the guest, and the
/// operations never overlap in time.
#[tokio::test]
async fn at_most_one_mutating_operation_is_open_per_workspace() {
    let agents = ["a1", "a2", "a3", "a4"];
    let (world, svc, ws) = shared_world(&agents).await;
    let ops: Vec<String> = (1..=agents.len()).map(|n| format!("op-{n}")).collect();
    let tasks: Vec<Task> = agents
        .iter()
        .zip(&ops)
        .map(|(agent, op)| spawn(&svc, ctx(op, agent), team(), SLOW))
        .collect();
    while !tasks.iter().all(JoinHandle::is_finished) {
        let open = world.repo.list_open_operations().await.unwrap();
        assert!(open.iter().filter(|op| op.workspace_id == ws).count() <= 1);
        assert!(
            guest_running(&world, &ws, &ops) <= 1,
            "two guest operations at once"
        );
        tokio::task::yield_now().await;
    }
    for task in tasks {
        assert_eq!(settled(&task.await.unwrap().unwrap()), SUCCEEDED);
    }
    let mut rows = world.repo.list_operations(&ws, 10).await.unwrap();
    rows.sort_by_key(|row| row.started_at);
    for pair in rows.windows(2) {
        assert!(pair[1].started_at >= pair[0].ended_at.unwrap());
    }
}

/// Operations alive in the guest across every generation's machine.
fn guest_running(world: &World, ws: &WorkspaceId, ops: &[String]) -> usize {
    let live = world.fake.live_machines();
    let mut running = 0;
    for (machine_ws, generation) in live.iter().filter(|(owner, _)| owner == ws) {
        for op in ops {
            let id = agentkit_workspaces::OperationId::new(op.as_str());
            let state = world.fake.operation_state(machine_ws, *generation, &id);
            running += usize::from(state.is_some_and(|state| !state.is_terminal()));
        }
    }
    running
}
