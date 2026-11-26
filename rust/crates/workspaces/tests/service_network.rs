//! The network request flow: a denial becomes a request, an approval or a
//! grant reaches a machine whose command is running, a revoke too, and a
//! rejected host is not asked again.

mod support;

use std::sync::Arc;

use agentkit_workspaces::{
    GrantSource, HostPort, NetworkDecision, OperationId, OperationOutcome, OperationPhase,
    OperationState, RequestState, SandboxError, WorkspaceRepository, WorkspaceService,
};
use support::{CURL, CURL_LATER, SUCCEEDED, TRUE, World, ctx, exec, home, settled};

type Task = tokio::task::JoinHandle<Result<OperationOutcome, SandboxError>>;

/// `CURL_LATER` as `op`, spawned; returns once the guest runs it.
async fn running_curl(world: &World, svc: &Arc<WorkspaceService>, op: &str) -> Task {
    let task = {
        let (svc, context) = (svc.clone(), ctx(op, "a1"));
        tokio::spawn(async move { svc.exec(&context, &home("a1"), exec(CURL_LATER)).await })
    };
    for _ in 0..100_000 {
        let row = world.repo.operation(&OperationId::new(op)).await.unwrap();
        if row.is_some_and(|row| row.phase == OperationPhase::Running) {
            return task;
        }
        tokio::task::yield_now().await;
    }
    panic!("{op} never ran");
}

/// A refused connection becomes a pending request; approving it grants
/// the host to the workspace and applies it live.
#[tokio::test]
async fn denial_becomes_a_request_and_approval_applies_live() {
    let world = World::new();
    let (svc, _) = world.service();
    let binding = home("a1");
    let denied = svc
        .exec(&ctx("op-1", "a1"), &binding, exec(CURL))
        .await
        .unwrap();
    let target = HostPort::new("example.com", 443).unwrap();
    assert_eq!(denied.network_denied, vec![target.clone()]);
    assert!(matches!(
        settled(&denied),
        OperationState::Failed { exit: Some(7), .. }
    ));
    let ws = world.workspace(&binding).await;
    let pending = svc.pending_requests(&ws.id).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(
        (
            pending[0].target.clone(),
            pending[0].operation_id.as_ref().map(|id| id.as_str())
        ),
        (target.clone(), Some("op-1"))
    );
    // A command is running when the owner approves: it connects after
    // the approval, on the policy the approval pushed live.
    let waiting = running_curl(&world, &svc, "op-2").await;
    let decided = svc
        .decide_request(&ws.id, &pending[0].id, NetworkDecision::Approve)
        .await
        .unwrap();
    assert_eq!(decided.request.state, RequestState::Approved);
    assert_eq!(
        decided.grant.map(|g| (g.target, g.source)),
        Some((target, GrantSource::Owner))
    );
    let allowed = waiting.await.unwrap().unwrap();
    assert_eq!(settled(&allowed), SUCCEEDED);
    assert!(allowed.network_denied.is_empty());
}

/// The owner's rejection stands: later denials count on the rejected
/// request and nobody is asked again.
#[tokio::test]
async fn a_rejected_host_is_not_asked_again() {
    let world = World::new();
    let (svc, _) = world.service();
    let binding = home("a1");
    svc.exec(&ctx("op-1", "a1"), &binding, exec(CURL))
        .await
        .unwrap();
    let ws = world.workspace(&binding).await;
    let request = svc.pending_requests(&ws.id).await.unwrap().remove(0);
    svc.decide_request(&ws.id, &request.id, NetworkDecision::Reject)
        .await
        .unwrap();
    let again = svc
        .exec(&ctx("op-2", "a1"), &binding, exec(CURL))
        .await
        .unwrap();
    assert_eq!(
        again.network_denied.len(),
        1,
        "the caller still learns host and port"
    );
    assert!(svc.pending_requests(&ws.id).await.unwrap().is_empty());
    let twice = svc
        .decide_request(&ws.id, &request.id, NetworkDecision::Approve)
        .await;
    assert!(twice.is_err(), "a decided request is not pending");
}

/// A grant and a revoke reach the machine while a command runs on it: the
/// command connects after the change and meets it.
#[tokio::test]
async fn grant_and_revoke_reach_the_running_machine() {
    let world = World::new();
    let (svc, _) = world.service();
    let binding = home("a1");
    svc.exec(&ctx("op-0", "a1"), &binding, exec(TRUE))
        .await
        .unwrap();
    let ws = world.workspace(&binding).await;
    let target = HostPort::new("example.com", 443).unwrap();
    let before_grant = running_curl(&world, &svc, "op-1").await;
    let grant = svc
        .grant(&ws.id, &target, GrantSource::Owner)
        .await
        .unwrap();
    let ok = before_grant.await.unwrap().unwrap();
    assert_eq!(settled(&ok), SUCCEEDED);
    let before_revoke = running_curl(&world, &svc, "op-2").await;
    svc.revoke(&grant.id).await.unwrap();
    let refused = before_revoke.await.unwrap().unwrap();
    assert_eq!(refused.network_denied, vec![target]);
}
