//! Bounds the service keeps whatever the backend or the model does: the
//! export's kind check under the listing bound, the pending-request cap,
//! the active-machine cap under concurrent starts, and a copy-in whose
//! name probe fails.

mod support;

use std::sync::atomic::Ordering::SeqCst;

use agentkit_testkit::Program;
use agentkit_workspaces::{
    AgentId, FileMode, FileRef, HostPort, LIST_MAX_ENTRIES, OperationId, PENDING_REQUESTS_MAX,
    SandboxError, WorkspaceRepository, WriteRequest,
};
use support::hooks::Hooked;
use support::{TRUE, World, ctx, exec, home, rel};

fn note(path: &str) -> WriteRequest {
    WriteRequest {
        file: FileRef::workspace(rel(path)),
        bytes: b"x".to_vec(),
        mode: FileMode::REGULAR,
    }
}

/// The export checks the file's kind in a bounded listing of its folder.
/// A file the listing cut off is refused, never answered as missing.
#[tokio::test]
async fn an_export_past_the_listing_bound_is_refused_not_missing() {
    let world = World::new();
    let (svc, _) = world.service();
    let binding = home("a1");
    for n in 0..=LIST_MAX_ENTRIES {
        let path = format!("big/f{n:04}.txt");
        svc.write_file(&ctx(&format!("op-{n}"), "a1"), &binding, note(&path))
            .await
            .unwrap();
    }
    let agent = AgentId::new("a1");
    let first = FileRef::workspace(rel("big/f0000.txt"));
    let export = svc.read_for_export(&binding, &agent, &first).await.unwrap();
    assert_eq!(export.bytes, b"x");
    let last = FileRef::workspace(rel(&format!("big/f{LIST_MAX_ENTRIES:04}.txt")));
    let refused = svc.read_for_export(&binding, &agent, &last).await;
    assert!(
        matches!(refused, Err(SandboxError::Unavailable { .. })),
        "{refused:?}"
    );
}

/// Denials of new targets stop becoming requests at the cap; the caller
/// still hears of each one.
#[tokio::test]
async fn pending_requests_stop_growing_at_the_cap() {
    let world = World::new();
    let (svc, _) = world.service();
    let binding = home("a1");
    for n in 0..PENDING_REQUESTS_MAX + 2 {
        let host = format!("h{n}.example.com");
        let command = format!("curl https://{host}/");
        world.fake.script(&command, Program::connect(&host, 443));
        let done = svc
            .exec(&ctx(&format!("op-{n}"), "a1"), &binding, exec(&command))
            .await
            .unwrap();
        assert_eq!(
            done.network_denied,
            vec![HostPort::new(&host, 443).unwrap()]
        );
    }
    let ws = world.workspace(&binding).await;
    let stored = world.repo.list_pending_requests(&ws.id).await.unwrap();
    assert_eq!(stored.len(), PENDING_REQUESTS_MAX);
    assert_eq!(
        svc.pending_requests(&ws.id).await.unwrap().len(),
        PENDING_REQUESTS_MAX
    );
}

/// Two first uses racing for the last machine slot: the cap check and the
/// start are one step, so the cap holds.
#[tokio::test]
async fn racing_starts_never_pass_the_machine_cap() {
    let mut world = World::new();
    world.config.max_active_machines = 1;
    let hooked = Hooked::new(world.backend.clone());
    let (svc, pause) = world.service_paused(hooked.clone());
    // The held start must not time out on virtual time.
    pause.freeze();
    hooked.gate_ensure.store(1, SeqCst);
    let first = {
        let svc = svc.clone();
        tokio::spawn(async move { svc.exec(&ctx("op-a", "a1"), &home("a1"), exec(TRUE)).await })
    };
    hooked.entered.notified().await;
    let second = {
        let svc = svc.clone();
        tokio::spawn(async move { svc.exec(&ctx("op-b", "a2"), &home("a2"), exec(TRUE)).await })
    };
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    hooked.release.notify_one();
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    pause.thaw();
    first.await.unwrap().unwrap();
    if let Err(error) = second.await.unwrap() {
        assert_eq!(error.code(), "sandbox_unavailable", "{error:?}");
    }
    let live = world.fake.live_machines();
    assert!(live.len() <= 1, "{live:?} run under a cap of one");
}

/// A copy-in's name probe that fails unexpectedly ends the call with that
/// error at once: it is not taken as "name in use" a thousand times.
#[tokio::test]
async fn a_failed_copy_in_probe_propagates() {
    let world = World::new();
    let hooked = Hooked::new(world.backend.clone());
    let svc = world.service_with(hooked.clone());
    let binding = home("a1");
    svc.exec(&ctx("op-0", "a1"), &binding, exec(TRUE))
        .await
        .unwrap();
    hooked.stale_reads.store(true, SeqCst);
    let before = hooked.reads.load(SeqCst);
    let refused = svc
        .copy_in(&ctx("op-1", "a1"), &binding, "a.txt", b"x".to_vec())
        .await;
    assert!(
        matches!(refused, Err(SandboxError::StaleGeneration { .. })),
        "{refused:?}"
    );
    assert_eq!(hooked.reads.load(SeqCst) - before, 1);
    assert_eq!(world.fake.dispatch_calls(&OperationId::new("op-1")), 0);
}
