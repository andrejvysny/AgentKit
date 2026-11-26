//! Members, network grants, and the denial-to-request flow.

use std::sync::Arc;

use agentkit_workspaces::{
    AgentId, DenialRecord, GrantId, HostPort, Member, MemberRole, NetworkGrant, NetworkRequest,
    OperationId, RepositoryError, RequestDecision, RequestId, RequestState, Workspace, WorkspaceId,
    WorkspaceRepository,
};

use super::{assert_invalid, create, grant, port};

fn member(workspace: &Workspace, agent: &str, added_at: i64) -> Member {
    Member {
        workspace_id: workspace.id.clone(),
        agent_id: AgentId::new(agent),
        role: MemberRole::Member,
        added_at,
    }
}

fn denial(id: &str, workspace: &WorkspaceId, host: &str, port: u16, seen_at: i64) -> DenialRecord {
    DenialRecord {
        id: RequestId::new(id),
        workspace_id: workspace.clone(),
        target: HostPort::new(host, port).expect("valid target"),
        operation_id: Some(OperationId::new(format!("op-{id}"))),
        seen_at,
        attempts: 1,
    }
}

fn not_found(workspace: &str) -> RepositoryError {
    RepositoryError::WorkspaceNotFound(WorkspaceId::new(workspace))
}

fn approve(grant: NetworkGrant) -> RequestDecision {
    RequestDecision::Approve { grant }
}

async fn record<T: WorkspaceRepository>(repo: &T, denial: &DenialRecord) -> NetworkRequest {
    port(repo.record_denial(denial).await)
}

pub(super) async fn members_add_remove_and_list<T: WorkspaceRepository>(repo: Arc<T>) {
    let ws = create(&*repo, "ws-1").await;
    let (a2, a1, a0) = (
        member(&ws, "a2", 20),
        member(&ws, "a1", 10),
        member(&ws, "a0", 20),
    );
    for added in [&a2, &a1, &a0] {
        assert_eq!(&port(repo.add_member(added).await), added);
    }
    let changed = Member {
        role: MemberRole::Reader,
        added_at: 99,
        ..a1.clone()
    };
    assert_eq!(port(repo.add_member(&changed).await), a1);
    assert_eq!(
        port(repo.list_members(&ws.id).await),
        vec![a1.clone(), a0.clone(), a2.clone()]
    );
    assert_eq!(
        port(repo.member(&ws.id, &a1.agent_id).await),
        Some(a1.clone())
    );
    assert!(port(repo.member(&ws.id, &AgentId::new("nope")).await).is_none());
    assert!(port(repo.remove_member(&ws.id, &a1.agent_id).await));
    assert!(!port(repo.remove_member(&ws.id, &a1.agent_id).await));
    assert_eq!(port(repo.list_members(&ws.id).await), vec![a0, a2]);
    let stray = Member {
        workspace_id: WorkspaceId::new("nope"),
        ..a1.clone()
    };
    assert_eq!(repo.add_member(&stray).await, Err(not_found("nope")));
    let removed = repo.remove_member(&stray.workspace_id, &a1.agent_id).await;
    assert!(!port(removed));
    assert!(port(repo.list_members(&stray.workspace_id).await).is_empty());
}

pub(super) async fn grants_insert_once_per_active_target<T: WorkspaceRepository>(repo: Arc<T>) {
    let ws = create(&*repo, "ws-1").await;
    let defaults = port(repo.list_active_grants(&ws.id).await);
    let first = grant("g-1", &ws.id, "example.com", 443, 200);
    assert_eq!(port(repo.insert_grant(&first).await), first);
    let duplicate = grant("g-2", &ws.id, "example.com", 443, 300);
    assert_eq!(port(repo.insert_grant(&duplicate).await), first);
    let other_port = grant("g-3", &ws.id, "example.com", 8443, 150);
    assert_eq!(port(repo.insert_grant(&other_port).await), other_port);
    let mut expected = defaults.clone();
    expected.extend([other_port.clone(), first.clone()]);
    assert_eq!(port(repo.list_active_grants(&ws.id).await), expected);
    let revoked = NetworkGrant {
        revoked_at: Some(1),
        ..grant("g-4", &ws.id, "crates.io", 443, 1)
    };
    assert_invalid(repo.insert_grant(&revoked).await);
    let stray = grant("g-5", &WorkspaceId::new("nope"), "crates.io", 443, 1);
    assert_eq!(repo.insert_grant(&stray).await, Err(not_found("nope")));
    // After a revoke the target takes a new grant.
    port(repo.revoke_grant(&first.id, 400).await);
    assert_eq!(port(repo.insert_grant(&duplicate).await), duplicate);
    let mut expected = defaults;
    expected.extend([other_port, duplicate]);
    assert_eq!(port(repo.list_active_grants(&ws.id).await), expected);
}

pub(super) async fn revoke_grant_is_idempotent<T: WorkspaceRepository>(repo: Arc<T>) {
    let ws = create(&*repo, "ws-1").await;
    let first = grant("g-1", &ws.id, "example.com", 443, 200);
    port(repo.insert_grant(&first).await);
    let revoked = port(repo.revoke_grant(&first.id, 500).await);
    assert_eq!(
        revoked,
        NetworkGrant {
            revoked_at: Some(500),
            ..first.clone()
        }
    );
    assert_eq!(port(repo.revoke_grant(&first.id, 900).await), revoked);
    let active = port(repo.list_active_grants(&ws.id).await);
    assert!(active.iter().all(|grant| grant.id != first.id));
    assert_eq!(active.len(), 3);
    let unknown = GrantId::new("nope");
    assert_eq!(
        repo.revoke_grant(&unknown, 1).await,
        Err(RepositoryError::GrantNotFound(unknown))
    );
}

pub(super) async fn denials_accumulate_on_the_pending_request<T: WorkspaceRepository>(
    repo: Arc<T>,
) {
    let ws = create(&*repo, "ws-1").await;
    let first = DenialRecord {
        attempts: 2,
        ..denial("r-1", &ws.id, "example.com", 443, 100)
    };
    let request = record(&*repo, &first).await;
    assert_eq!(
        request,
        NetworkRequest {
            id: first.id.clone(),
            workspace_id: ws.id.clone(),
            target: first.target.clone(),
            operation_id: first.operation_id.clone(),
            first_seen: 100,
            count: 2,
            state: RequestState::Pending,
        }
    );
    let second = DenialRecord {
        attempts: 3,
        ..denial("r-2", &ws.id, "example.com", 443, 200)
    };
    let grown = record(&*repo, &second).await;
    assert_eq!(
        (grown.id.as_str(), grown.count, grown.first_seen),
        ("r-1", 5, 100)
    );
    assert_eq!(grown.operation_id, second.operation_id);
    // A denial without an operation keeps the last known one.
    let anonymous = DenialRecord {
        operation_id: None,
        ..denial("r-3", &ws.id, "example.com", 443, 300)
    };
    let kept = record(&*repo, &anonymous).await;
    assert_eq!((kept.count, kept.operation_id), (6, second.operation_id));
    let other = record(&*repo, &denial("r-4", &ws.id, "example.com", 8443, 50)).await;
    let pending = port(repo.list_pending_requests(&ws.id).await);
    let ids: Vec<_> = pending.iter().map(|request| request.id.as_str()).collect();
    assert_eq!(ids, ["r-4", "r-1"]);
    assert_eq!(pending[0], other);
    let neighbour = create(&*repo, "ws-2").await;
    let own = record(&*repo, &denial("r-7", &neighbour.id, "example.com", 443, 1)).await;
    assert_eq!((own.id.as_str(), own.count), ("r-7", 1));
    assert_eq!(port(repo.list_pending_requests(&ws.id).await).len(), 2);
}

pub(super) async fn record_denial_refuses_zero_attempts_and_strays<T: WorkspaceRepository>(
    repo: Arc<T>,
) {
    let ws = create(&*repo, "ws-1").await;
    let zero = DenialRecord {
        attempts: 0,
        ..denial("r-5", &ws.id, "example.com", 443, 400)
    };
    assert_invalid(repo.record_denial(&zero).await);
    let stray = denial("r-6", &WorkspaceId::new("nope"), "example.com", 443, 1);
    assert_eq!(repo.record_denial(&stray).await, Err(not_found("nope")));
    assert!(port(repo.list_pending_requests(&ws.id).await).is_empty());
}

/// Owner decision: a rejected target is not asked about again; its denials
/// count on the rejected request, which the owner can still grant later.
pub(super) async fn denial_after_rejection_counts_on_the_rejected_one<T: WorkspaceRepository>(
    repo: Arc<T>,
) {
    let ws = create(&*repo, "ws-1").await;
    let first = record(&*repo, &denial("r-1", &ws.id, "example.com", 443, 100)).await;
    let decided = port(
        repo.decide_request(&first.id, &RequestDecision::Reject)
            .await,
    );
    assert_eq!(
        (decided.request.state, decided.grant),
        (RequestState::Rejected, None)
    );
    let again = DenialRecord {
        attempts: 2,
        ..denial("r-2", &ws.id, "example.com", 443, 200)
    };
    let counted = record(&*repo, &again).await;
    assert_eq!(
        counted,
        NetworkRequest {
            count: 3,
            operation_id: again.operation_id.clone(),
            state: RequestState::Rejected,
            ..first.clone()
        }
    );
    assert!(port(repo.list_pending_requests(&ws.id).await).is_empty());
    assert_eq!(
        repo.decide_request(&again.id, &RequestDecision::Reject)
            .await,
        Err(RepositoryError::RequestNotFound(again.id.clone()))
    );
    assert_eq!(
        repo.decide_request(&first.id, &RequestDecision::Reject)
            .await,
        Err(RepositoryError::RequestNotPending {
            request_id: first.id.clone(),
            state: RequestState::Rejected,
        })
    );
    let later = grant("g-1", &ws.id, "example.com", 443, 300);
    assert_eq!(port(repo.insert_grant(&later).await), later);
}

pub(super) async fn denial_after_approval_opens_a_new_request<T: WorkspaceRepository>(
    repo: Arc<T>,
) {
    let ws = create(&*repo, "ws-1").await;
    let first = record(&*repo, &denial("r-1", &ws.id, "example.com", 443, 100)).await;
    let allowed = grant("g-1", &ws.id, "example.com", 443, 150);
    port(
        repo.decide_request(&first.id, &approve(allowed.clone()))
            .await,
    );
    port(repo.revoke_grant(&allowed.id, 160).await);
    let again = DenialRecord {
        attempts: 4,
        ..denial("r-2", &ws.id, "example.com", 443, 200)
    };
    let fresh = record(&*repo, &again).await;
    assert_eq!(
        (
            fresh.id.as_str(),
            fresh.first_seen,
            fresh.count,
            fresh.state
        ),
        ("r-2", 200, 4, RequestState::Pending)
    );
    assert_eq!(port(repo.list_pending_requests(&ws.id).await), vec![fresh]);
}

pub(super) async fn approve_inserts_the_grant_with_the_decision<T: WorkspaceRepository>(
    repo: Arc<T>,
) {
    let ws = create(&*repo, "ws-1").await;
    let request = record(&*repo, &denial("r-1", &ws.id, "example.com", 443, 100)).await;
    let allowed = grant("g-1", &ws.id, "example.com", 443, 300);
    let decided = port(
        repo.decide_request(&request.id, &approve(allowed.clone()))
            .await,
    );
    assert_eq!(
        decided.request,
        NetworkRequest {
            state: RequestState::Approved,
            ..request
        }
    );
    assert_eq!(decided.grant, Some(allowed.clone()));
    assert!(port(repo.list_active_grants(&ws.id).await).contains(&allowed));
    assert!(port(repo.list_pending_requests(&ws.id).await).is_empty());
    // An existing active grant for the target is reused.
    let defaults = port(repo.list_active_grants(&ws.id).await);
    let pypi = defaults
        .iter()
        .find(|grant| grant.target.host() == "pypi.org")
        .expect("default grant")
        .clone();
    let second = record(&*repo, &denial("r-2", &ws.id, "pypi.org", 443, 400)).await;
    let duplicate = grant("g-2", &ws.id, "pypi.org", 443, 500);
    let reused = port(repo.decide_request(&second.id, &approve(duplicate)).await);
    assert_eq!(reused.grant, Some(pypi));
    assert_eq!(reused.request.state, RequestState::Approved);
    assert_eq!(port(repo.list_active_grants(&ws.id).await), defaults);
}

pub(super) async fn decide_refuses_unknown_decided_and_mismatched<T: WorkspaceRepository>(
    repo: Arc<T>,
) {
    let ws = create(&*repo, "ws-1").await;
    let unknown = RequestId::new("nope");
    assert_eq!(
        repo.decide_request(&unknown, &RequestDecision::Reject)
            .await,
        Err(RepositoryError::RequestNotFound(unknown))
    );
    let request = record(&*repo, &denial("r-1", &ws.id, "example.com", 443, 100)).await;
    let other = create(&*repo, "ws-2").await;
    let refusals = [
        grant("g-1", &ws.id, "other.example", 443, 200),
        grant("g-1", &ws.id, "example.com", 80, 200),
        grant("g-1", &other.id, "example.com", 443, 200),
        NetworkGrant {
            revoked_at: Some(200),
            ..grant("g-1", &ws.id, "example.com", 443, 200)
        },
    ];
    let before = port(repo.list_active_grants(&ws.id).await);
    for refused in refusals {
        assert_invalid(repo.decide_request(&request.id, &approve(refused)).await);
    }
    assert_eq!(
        port(repo.list_pending_requests(&ws.id).await),
        vec![request.clone()]
    );
    assert_eq!(port(repo.list_active_grants(&ws.id).await), before);
    assert_eq!(port(repo.list_active_grants(&other.id).await).len(), 3);
    port(
        repo.decide_request(&request.id, &RequestDecision::Reject)
            .await,
    );
    let late = grant("g-1", &ws.id, "example.com", 443, 200);
    assert_eq!(
        repo.decide_request(&request.id, &approve(late)).await,
        Err(RepositoryError::RequestNotPending {
            request_id: request.id.clone(),
            state: RequestState::Rejected,
        })
    );
}
