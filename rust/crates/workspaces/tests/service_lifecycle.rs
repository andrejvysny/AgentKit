//! Machine lifecycle: stop, idle stop and the active-machine cap, reset of
//! `/home/agent`, delete, the quota (F-11) and a stale generation served
//! by another host.

mod support;

use agentkit_testkit::DISK_FULL_EXIT;
use agentkit_workspaces::{
    FileMode, FileRef, Generation, MachineSpec, NetworkPolicy, OperationState, PathRoot,
    SandboxBackend, SandboxError, StopOutcome, WorkspaceRepository, WorkspaceState, WriteRequest,
};
use support::{LONG, SUCCEEDED, TRUE, World, assert_fenced, ctx, exec, home, rel, settled};

fn write(root: PathRoot, path: &str, bytes: &[u8]) -> WriteRequest {
    WriteRequest {
        file: FileRef {
            root,
            path: rel(path),
        },
        bytes: bytes.to_vec(),
        mode: FileMode::REGULAR,
    }
}

/// A proven stop advances the generation at once; the next use starts a
/// fresh machine. Stopping a generation that never started changes
/// nothing and fences nothing.
#[tokio::test]
async fn stop_proves_advances_and_the_next_use_starts_fresh() {
    let world = World::new();
    let (svc, _) = world.service();
    svc.exec(&ctx("op-1", "a1"), &home("a1"), exec(TRUE))
        .await
        .unwrap();
    let ws = world.workspace(&home("a1")).await;
    let stopped = svc.stop(&ws.id).await.unwrap();
    assert_eq!(
        stopped,
        StopOutcome::Stopped {
            next_generation: Generation(2)
        }
    );
    assert!(world.fake.live_machines().is_empty());
    assert_eq!(svc.stop(&ws.id).await.unwrap(), StopOutcome::NotStarted);
    assert_eq!(
        world.workspace(&home("a1")).await.state,
        WorkspaceState::Ready
    );
    let next = svc
        .exec(&ctx("op-2", "a1"), &home("a1"), exec(TRUE))
        .await
        .unwrap();
    assert_eq!(next.generation, Generation(2));
    assert_eq!(world.generations_proven().await, Vec::<String>::new());
}

/// Idle machines are stopped least recently used first; a busy one (its
/// lease held) never.
#[tokio::test]
async fn idle_stop_spares_a_busy_machine() {
    let mut world = World::new();
    world.config.idle_stop_ms = 5_000;
    let (svc, _) = world.service();
    svc.exec(&ctx("op-1", "a1"), &home("a1"), exec(TRUE))
        .await
        .unwrap();
    let busy = {
        let svc = svc.clone();
        tokio::spawn(async move { svc.exec(&ctx("op-2", "a2"), &home("a2"), exec(LONG)).await })
    };
    while world.fake.live_machines().len() < 2 {
        tokio::task::yield_now().await;
    }
    // Time passes while the busy command keeps renewing its lease.
    let idle_since = world.now();
    while world.now() <= idle_since + world.config.idle_stop_ms {
        world.clock.advance(100);
        tokio::task::yield_now().await;
    }
    let stopped = svc.evict_idle(world.now()).await.unwrap();
    let idle = world.workspace(&home("a1")).await;
    assert_eq!(stopped, vec![idle.id.clone()]);
    let live: Vec<_> = world
        .fake
        .live_machines()
        .into_iter()
        .map(|(ws, _)| ws)
        .collect();
    assert_eq!(live, vec![world.workspace(&home("a2")).await.id]);
    assert_eq!(settled(&busy.await.unwrap().unwrap()), SUCCEEDED);
}

/// The active-machine cap stops the least recently used idle machine to
/// make room, never a busy one; with every machine busy the call is
/// refused as unavailable.
#[tokio::test]
async fn the_machine_cap_evicts_the_least_recently_used_idle_machine() {
    let world = World::new();
    let (svc, _) = world.service();
    for (op, agent) in [
        ("op-1", "a1"),
        ("op-2", "a2"),
        ("op-3", "a1"),
        ("op-4", "a3"),
    ] {
        world.clock.advance(1_000);
        svc.exec(&ctx(op, agent), &home(agent), exec(TRUE))
            .await
            .unwrap();
    }
    let a2 = world.workspace(&home("a2")).await;
    assert_eq!(a2.generation, Generation(2), "a2 was least recently used");
    assert_eq!(world.fake.live_machines().len(), 2);
    let spawn = |op: &'static str, agent: &'static str| {
        let svc = svc.clone();
        tokio::spawn(async move { svc.exec(&ctx(op, agent), &home(agent), exec(LONG)).await })
    };
    let (one, three) = (spawn("op-5", "a1"), spawn("op-6", "a3"));
    while world.repo.list_open_operations().await.unwrap().len() < 2 {
        tokio::task::yield_now().await;
    }
    let refused = svc.exec(&ctx("op-7", "a4"), &home("a4"), exec(TRUE)).await;
    assert_eq!(refused.unwrap_err().code(), "sandbox_unavailable");
    for task in [one, three] {
        assert_eq!(settled(&task.await.unwrap().unwrap()), SUCCEEDED);
    }
    assert_eq!(world.generations_proven().await, Vec::<String>::new());
}

/// Reset of installed tools: `/home/agent` emptied, `/workspace` kept.
#[tokio::test]
async fn reset_home_empties_home_and_keeps_the_workspace() {
    let world = World::new();
    let (svc, _) = world.service();
    let binding = home("a1");
    let home_file = write(PathRoot::Home, ".local/bin/tool", b"tool");
    svc.write_file(&ctx("op-1", "a1"), &binding, home_file)
        .await
        .unwrap();
    let kept = write(PathRoot::Workspace, "notes.md", b"kept");
    svc.write_file(&ctx("op-2", "a1"), &binding, kept)
        .await
        .unwrap();
    let ws = world.workspace(&binding).await;
    svc.reset_home(&ws.id).await.unwrap();
    assert_eq!(world.fake.home_resets(&ws.id), 1);
    assert_eq!(world.fake.home_file(&ws.id, ".local/bin/tool"), None);
    assert_eq!(
        world.fake.volume_file(&ws.id, "notes.md"),
        Some(b"kept".to_vec())
    );
    let next = svc
        .exec(&ctx("op-3", "a1"), &binding, exec(TRUE))
        .await
        .unwrap();
    assert_eq!(next.generation, Generation(2));
}

/// Delete: proven stop, storage destroyed, record kept as deleted and the
/// key usable again. An unproven stop keeps the record, fenced.
#[tokio::test]
async fn delete_needs_a_proven_stop_and_frees_the_key() {
    let world = World::new();
    let (svc, _) = world.service();
    let binding = home("a1");
    let file = write(PathRoot::Workspace, "a.txt", b"a");
    svc.write_file(&ctx("op-1", "a1"), &binding, file)
        .await
        .unwrap();
    let ws = world.workspace(&binding).await;
    world.fake.refuse_stop(true);
    assert_fenced(svc.delete(&ws.id).await);
    let kept = world.repo.workspace(&ws.id).await.unwrap().unwrap();
    assert!(matches!(kept.state, WorkspaceState::Fenced { .. }));
    world.fake.refuse_stop(false);
    svc.delete(&ws.id).await.unwrap();
    let gone = world.repo.workspace(&ws.id).await.unwrap().unwrap();
    assert_eq!(gone.state, WorkspaceState::Deleted);
    assert_eq!(world.fake.volume_file(&ws.id, "a.txt"), None);
    let again = svc.resolve(&binding).await.unwrap();
    assert_ne!(again.id, ws.id);
    let deleted = svc.delete(&ws.id).await;
    assert!(
        matches!(deleted, Err(SandboxError::NotFound { .. })),
        "{deleted:?}"
    );
}

/// F-11: the disk fills during a command; the command fails, and the next
/// mutating call answers quota_exceeded before anything is dispatched.
/// Reads still work.
#[tokio::test]
async fn a_full_disk_fails_the_command_and_refuses_the_next_write() {
    let world = World::new();
    let (svc, _) = world.service();
    let binding = home("a1");
    let note = write(PathRoot::Workspace, "note.txt", b"kept");
    svc.write_file(&ctx("op-0", "a1"), &binding, note)
        .await
        .unwrap();
    let running = {
        let svc = svc.clone();
        tokio::spawn(async move { svc.exec(&ctx("op-1", "a1"), &home("a1"), exec(LONG)).await })
    };
    while world.fake.live_machines().is_empty()
        || world.repo.list_open_operations().await.unwrap().is_empty()
    {
        tokio::task::yield_now().await;
    }
    let ws = world.workspace(&binding).await;
    world.fake.fill_disk(&ws.id);
    let failed = running.await.unwrap().unwrap();
    let full = OperationState::Failed {
        exit: Some(DISK_FULL_EXIT),
        signal: None,
    };
    assert_eq!(settled(&failed), full);
    let dispatched = world.fake.dispatch_calls_total();
    let refused = svc.exec(&ctx("op-2", "a1"), &binding, exec(TRUE)).await;
    assert_eq!(refused.unwrap_err().code(), "quota_exceeded");
    assert_eq!(world.fake.dispatch_calls_total(), dispatched);
    let agent = agentkit_workspaces::AgentId::new("a1");
    let status = svc.status(&binding, &agent).await.unwrap();
    assert!(status.storage.unwrap().is_exhausted());
    let file = FileRef::workspace(rel("note.txt"));
    let read = svc
        .read_file(&binding, &agent, &file, 0, 100)
        .await
        .unwrap();
    assert_eq!(read.bytes, b"kept");
}

/// The backend serves a newer generation than the record (another host
/// started it): the workspace is fenced with that generation recorded and
/// nothing is dispatched. Recover proves the served generation stopped and
/// moves past it; the next call runs on a fresh generation.
#[tokio::test]
async fn a_newer_generation_served_elsewhere_fences() {
    let world = World::new();
    let (svc, _) = world.service();
    let ws = svc.resolve(&home("a1")).await.unwrap();
    // Another host starts generation 5 of the same workspace.
    let spec = MachineSpec {
        workspace_id: ws.id.clone(),
        generation: Generation(5),
        image_digest: ws.image_digest.clone(),
        limits: world.config.limits,
        network: NetworkPolicy::default(),
    };
    world.fake.ensure_machine(&spec).await.unwrap();
    assert_fenced(svc.exec(&ctx("op-1", "a1"), &home("a1"), exec(TRUE)).await);
    assert_eq!(world.fake.dispatch_calls_total(), 0);
    let fenced = world.workspace(&home("a1")).await;
    assert!(
        matches!(
            fenced.state,
            WorkspaceState::Fenced {
                generation: Generation(5),
                ..
            }
        ),
        "{:?}",
        fenced.state
    );
    let recovered = svc.recover(&ws.id).await.unwrap();
    assert_eq!(
        recovered,
        StopOutcome::Stopped {
            next_generation: Generation(6)
        }
    );
    let done = svc
        .exec(&ctx("op-2", "a1"), &home("a1"), exec(TRUE))
        .await
        .unwrap();
    assert_eq!(
        (settled(&done), done.generation),
        (SUCCEEDED, Generation(6))
    );
    assert!(world.generations_proven().await.is_empty());
}
