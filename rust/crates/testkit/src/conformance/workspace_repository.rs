//! Conformance suite for `agentkit_workspaces::WorkspaceRepository` (ADR
//! 0038): the behaviour every implementation (the in-memory twin here, the
//! SQLite adapter in `agentkit-store`) must share. Each case runs on a
//! repository built fresh by `run`. Faults are matched on
//! `RepositoryError` variants. Where the port leaves a precedence open,
//! the suite pins one, so both implementations answer alike.

use std::fmt::Debug;
use std::sync::Arc;

use agentkit_workspaces::{
    AgentId, DEFAULT_GRANTS, Generation, GrantId, GrantSource, HostPort, LeaseAcquire, LeaseHolder,
    LeaseToken, NetworkGrant, NewWorkspace, Operation, OperationId, OperationKindTag,
    OperationPhase, RepositoryError, Sha256, Workspace, WorkspaceId, WorkspaceKey, WorkspaceKind,
    WorkspaceRepository, WorkspaceState,
};

use super::case_support::port;

mod network;
mod operations;
mod recovery;
mod states;

use network::{
    approve_inserts_the_grant_with_the_decision, decide_refuses_unknown_decided_and_mismatched,
    denial_after_approval_opens_a_new_request, denial_after_rejection_counts_on_the_rejected_one,
    denials_accumulate_on_the_pending_request, grants_insert_once_per_active_target,
    members_add_remove_and_list, record_denial_refuses_zero_attempts_and_strays,
    revoke_grant_is_idempotent,
};
use operations::{
    insert_operation_checks_in_documented_order, insert_operation_needs_a_ready_workspace,
    insert_operation_replays_and_conflicts, insert_operation_turns_ready_into_busy,
    operations_list_newest_first_and_open_only, update_operation_checks_lease_then_row,
    update_operation_follows_phase_rules, update_operation_keeps_unset_fields,
};
use recovery::{
    advance_and_unfence_moves_past_the_fence_at_once, advance_and_unfence_needs_no_open_operation,
    fence_records_the_newest_generation_that_may_run,
};
use states::{
    advance_generation_compares_and_swaps, fence_and_unfence_round_trip,
    holds_user_files_is_refused_on_deleted, mark_deleted_needs_ready_or_fenced,
    mark_ready_refuses_while_an_operation_is_open,
    mark_ready_refuses_while_the_current_generation_is_uncertain,
    open_operation_blocks_unfence_and_advance, stale_token_changes_nothing,
};

suite!(
    "workspace repository",
    WorkspaceRepository,
    create_or_get_creates_once_and_seeds_grants,
    create_or_get_returns_existing_and_ignores_its_input,
    create_or_get_refuses_invalid_initial_grants,
    concurrent_first_use_creates_one_workspace,
    deleted_key_creates_a_new_record,
    list_workspaces_orders_by_creation_then_id,
    touch_moves_last_used_forward_only,
    lease_answers_held_until_expiry,
    renew_extends_a_current_lease_only,
    release_drops_only_the_matching_lease,
    fencing_increases_across_acquire_release_and_break,
    break_lease_matches_the_fencing_number,
    stale_token_changes_nothing,
    mark_ready_refuses_while_an_operation_is_open,
    mark_ready_refuses_while_the_current_generation_is_uncertain,
    open_operation_blocks_unfence_and_advance,
    fence_and_unfence_round_trip,
    advance_generation_compares_and_swaps,
    fence_records_the_newest_generation_that_may_run,
    advance_and_unfence_moves_past_the_fence_at_once,
    advance_and_unfence_needs_no_open_operation,
    mark_deleted_needs_ready_or_fenced,
    holds_user_files_is_refused_on_deleted,
    insert_operation_turns_ready_into_busy,
    insert_operation_replays_and_conflicts,
    insert_operation_checks_in_documented_order,
    insert_operation_needs_a_ready_workspace,
    update_operation_follows_phase_rules,
    update_operation_keeps_unset_fields,
    update_operation_checks_lease_then_row,
    operations_list_newest_first_and_open_only,
    members_add_remove_and_list,
    grants_insert_once_per_active_target,
    revoke_grant_is_idempotent,
    denials_accumulate_on_the_pending_request,
    record_denial_refuses_zero_attempts_and_strays,
    denial_after_rejection_counts_on_the_rejected_one,
    denial_after_approval_opens_a_new_request,
    approve_inserts_the_grant_with_the_decision,
    decide_refuses_unknown_decided_and_mismatched,
);

/// Lease time-to-live every fixture lease uses.
const TTL: i64 = 1_000;
/// When fixture leases are taken; they expire at `ACQUIRED + TTL`.
const ACQUIRED: i64 = 1_000;
/// A moment inside the fixture lease.
const DURING: i64 = 1_500;
const DIGEST: &str = "sha256:image-1";

fn grant(id: &str, workspace: &WorkspaceId, host: &str, port: u16, at: i64) -> NetworkGrant {
    NetworkGrant {
        id: GrantId::new(id),
        workspace_id: workspace.clone(),
        target: HostPort::new(host, port).expect("valid target"),
        source: GrantSource::Owner,
        granted_at: at,
        revoked_at: None,
    }
}

/// The default grants of `id`, with ids `<id>-grant-<n>`.
fn default_grants(id: &WorkspaceId, at: i64) -> Vec<NetworkGrant> {
    DEFAULT_GRANTS
        .iter()
        .enumerate()
        .map(|(n, (host, port))| NetworkGrant {
            source: GrantSource::Default,
            ..grant(&format!("{id}-grant-{n}"), id, host, *port, at)
        })
        .collect()
}

fn new_workspace(id: &str, key: &str, now: i64) -> NewWorkspace {
    let id = WorkspaceId::new(id);
    NewWorkspace {
        initial_grants: default_grants(&id, now),
        id,
        kind: WorkspaceKind::AgentHome,
        key: WorkspaceKey::new(key),
        owner_scope: "owner:1".to_owned(),
        image_digest: DIGEST.to_owned(),
        now,
    }
}

async fn create_at<T: WorkspaceRepository>(repo: &T, id: &str, key: &str, now: i64) -> Workspace {
    let upsert = port(
        repo.create_or_get_workspace(&new_workspace(id, key, now))
            .await,
    );
    assert!(upsert.created, "{id} was created");
    upsert.workspace
}

async fn create<T: WorkspaceRepository>(repo: &T, id: &str) -> Workspace {
    create_at(repo, id, &format!("key:{id}"), 100).await
}

fn holder(agent: &str) -> LeaseHolder {
    LeaseHolder {
        agent_id: AgentId::new(agent),
        run_id: format!("run-{agent}"),
        label: format!("Agent {agent}"),
        host_id: "host-1".to_owned(),
        boot_id: "boot-1".to_owned(),
    }
}

async fn acquire_at<T: WorkspaceRepository>(
    repo: &T,
    workspace: &WorkspaceId,
    token: &str,
    now: i64,
) -> LeaseToken {
    match port(
        repo.acquire_lease(workspace, &holder("a1"), token, now, TTL)
            .await,
    ) {
        LeaseAcquire::Acquired(lease) => lease,
        held => panic!("expected the lease, got {held:?}"),
    }
}

async fn acquire<T: WorkspaceRepository>(repo: &T, workspace: &WorkspaceId) -> LeaseToken {
    acquire_at(repo, workspace, "token-1", ACQUIRED).await
}

fn dispatching(id: &str, workspace: &Workspace, started_at: i64) -> Operation {
    Operation {
        id: OperationId::new(id),
        workspace_id: workspace.id.clone(),
        generation: workspace.generation,
        kind: OperationKindTag::Exec,
        actor_agent_id: AgentId::new("a1"),
        run_id: "run-a1".to_owned(),
        phase: OperationPhase::Dispatching,
        started_at,
        ended_at: None,
        log_path: None,
        truncated: false,
        image_digest: workspace.image_digest.clone(),
        summary: "make test".to_owned(),
        args_sha256: Sha256::of(id.as_bytes()),
    }
}

/// A workspace, its lease, and one open operation (so it is `Busy`).
async fn busy<T: WorkspaceRepository>(repo: &T, id: &str) -> (Workspace, LeaseToken, Operation) {
    let workspace = create(repo, id).await;
    let lease = acquire(repo, &workspace.id).await;
    let op = dispatching(&format!("{id}-op-1"), &workspace, DURING);
    assert!(port(repo.insert_operation(&lease, &op, DURING).await));
    let workspace = port(repo.workspace(&workspace.id).await).expect("workspace");
    (workspace, lease, op)
}

fn assert_lost<T: Debug>(outcome: Result<T, RepositoryError>, workspace: &WorkspaceId) {
    match outcome {
        Err(RepositoryError::LeaseLost { workspace_id }) => assert_eq!(&workspace_id, workspace),
        other => panic!("expected LeaseLost, got {other:?}"),
    }
}

fn assert_state_conflict<T: Debug>(outcome: Result<T, RepositoryError>, state: WorkspaceState) {
    match outcome {
        Err(RepositoryError::WorkspaceStateConflict { state: found, .. }) => {
            assert_eq!(found, state)
        }
        other => panic!("expected WorkspaceStateConflict, got {other:?}"),
    }
}

fn assert_invalid<T: Debug>(outcome: Result<T, RepositoryError>) {
    assert!(
        matches!(outcome, Err(RepositoryError::InvalidInput { .. })),
        "{outcome:?}"
    );
}

async fn create_or_get_creates_once_and_seeds_grants<T: WorkspaceRepository>(repo: Arc<T>) {
    let new = new_workspace("ws-1", "agent:a1", 100);
    let upsert = port(repo.create_or_get_workspace(&new).await);
    assert!(upsert.created);
    let ws = upsert.workspace;
    assert_eq!((ws.id.as_str(), ws.key.as_str()), ("ws-1", "agent:a1"));
    assert_eq!(
        (ws.kind, ws.owner_scope.as_str()),
        (WorkspaceKind::AgentHome, "owner:1")
    );
    assert_eq!(
        (ws.state.clone(), ws.generation),
        (WorkspaceState::Ready, Generation::FIRST)
    );
    assert_eq!(
        (ws.image_digest.as_str(), ws.holds_user_files),
        (DIGEST, false)
    );
    assert_eq!((ws.created_at, ws.last_used_at), (100, 100));
    assert_eq!(port(repo.workspace(&ws.id).await), Some(ws.clone()));
    assert_eq!(port(repo.workspace_by_key(&ws.key).await), Some(ws.clone()));
    assert_eq!(
        port(repo.list_active_grants(&ws.id).await),
        new.initial_grants
    );
    assert!(port(repo.lease(&ws.id, 100).await).is_none());
}

async fn create_or_get_returns_existing_and_ignores_its_input<T: WorkspaceRepository>(
    repo: Arc<T>,
) {
    let first = create_at(&*repo, "ws-1", "agent:a1", 100).await;
    let mut again = new_workspace("ws-2", "agent:a1", 200);
    again.kind = WorkspaceKind::Shared;
    // A losing caller's grants are ignored, not validated.
    again.initial_grants = vec![grant(
        "g-x",
        &WorkspaceId::new("elsewhere"),
        "example.com",
        443,
        200,
    )];
    let upsert = port(repo.create_or_get_workspace(&again).await);
    assert!(!upsert.created);
    assert_eq!(upsert.workspace, first);
    assert!(port(repo.workspace(&again.id).await).is_none());
    assert_eq!(port(repo.list_active_grants(&first.id).await).len(), 3);
    assert_eq!(port(repo.list_workspaces().await), vec![first]);
}

async fn create_or_get_refuses_invalid_initial_grants<T: WorkspaceRepository>(repo: Arc<T>) {
    let mut foreign = new_workspace("ws-1", "agent:a1", 100);
    foreign.initial_grants[1].workspace_id = WorkspaceId::new("elsewhere");
    assert_invalid(repo.create_or_get_workspace(&foreign).await);
    let mut revoked = new_workspace("ws-1", "agent:a1", 100);
    revoked.initial_grants[0].revoked_at = Some(100);
    assert_invalid(repo.create_or_get_workspace(&revoked).await);
    assert!(port(repo.list_workspaces().await).is_empty());
    assert!(port(repo.workspace_by_key(&foreign.key).await).is_none());
    assert!(port(repo.list_active_grants(&foreign.id).await).is_empty());
}

/// F-17: concurrent first uses of one key create exactly one record and
/// seed its default grants once.
async fn concurrent_first_use_creates_one_workspace<T: WorkspaceRepository + 'static>(
    repo: Arc<T>,
) {
    let callers: Vec<_> = (0..8)
        .map(|n| {
            let repo = repo.clone();
            let new = new_workspace(&format!("ws-{n}"), "chats:project:p1", 100 + n);
            tokio::spawn(async move { repo.create_or_get_workspace(&new).await })
        })
        .collect();
    let mut upserts = Vec::new();
    for caller in callers {
        upserts.push(port(caller.await.expect("caller task")));
    }
    let winners: Vec<_> = upserts.iter().filter(|upsert| upsert.created).collect();
    assert_eq!(winners.len(), 1, "exactly one caller creates");
    let id = winners[0].workspace.id.clone();
    assert!(upserts.iter().all(|upsert| upsert.workspace.id == id));
    assert_eq!(port(repo.list_workspaces().await).len(), 1);
    let grants = port(repo.list_active_grants(&id).await);
    assert_eq!(grants, default_grants(&id, winners[0].workspace.created_at));
}

async fn deleted_key_creates_a_new_record<T: WorkspaceRepository>(repo: Arc<T>) {
    let old = create_at(&*repo, "ws-1", "agent:a1", 100).await;
    let lease = acquire(&*repo, &old.id).await;
    port(repo.mark_deleted(&lease, DURING).await);
    assert!(port(repo.workspace_by_key(&old.key).await).is_none());
    let new = create_at(&*repo, "ws-2", "agent:a1", 200).await;
    assert_eq!(new.id.as_str(), "ws-2");
    let kept = port(repo.workspace(&old.id).await).expect("deleted record stays");
    assert_eq!((kept.state, kept.key), (WorkspaceState::Deleted, old.key));
    assert_eq!(
        port(repo.workspace_by_key(&new.key).await),
        Some(new.clone())
    );
    let ids: Vec<_> = port(repo.list_workspaces().await)
        .into_iter()
        .map(|ws| ws.id)
        .collect();
    assert_eq!(ids, vec![old.id, new.id]);
}

async fn list_workspaces_orders_by_creation_then_id<T: WorkspaceRepository>(repo: Arc<T>) {
    create_at(&*repo, "b", "key:b", 100).await;
    create_at(&*repo, "a", "key:a", 100).await;
    create_at(&*repo, "c", "key:c", 50).await;
    let ids: Vec<_> = port(repo.list_workspaces().await)
        .into_iter()
        .map(|ws| ws.id.as_str().to_owned())
        .collect();
    assert_eq!(ids, ["c", "a", "b"]);
}

async fn touch_moves_last_used_forward_only<T: WorkspaceRepository>(repo: Arc<T>) {
    let ws = create(&*repo, "ws-1").await;
    port(repo.touch_workspace(&ws.id, 500).await);
    port(repo.touch_workspace(&ws.id, 300).await);
    let stored = port(repo.workspace(&ws.id).await).expect("workspace");
    assert_eq!((stored.created_at, stored.last_used_at), (100, 500));
    let unknown = repo.touch_workspace(&WorkspaceId::new("nope"), 500).await;
    assert_eq!(
        unknown,
        Err(RepositoryError::WorkspaceNotFound(WorkspaceId::new("nope")))
    );
}

async fn lease_answers_held_until_expiry<T: WorkspaceRepository>(repo: Arc<T>) {
    let ws = create(&*repo, "ws-1").await;
    let first = acquire(&*repo, &ws.id).await;
    assert_eq!(
        (first.token.as_str(), first.expires_at),
        ("token-1", ACQUIRED + TTL)
    );
    let info = port(repo.lease(&ws.id, ACQUIRED).await).expect("lease");
    assert_eq!((info.holder, info.fencing), (holder("a1"), first.fencing));
    // The same holder is answered `Held` too.
    let again = port(
        repo.acquire_lease(&ws.id, &holder("a1"), "token-2", 1_999, TTL)
            .await,
    );
    assert_eq!(
        again,
        LeaseAcquire::Held {
            holder: holder("a1"),
            expires_at: ACQUIRED + TTL
        }
    );
    // `expires_at == now` is expired.
    assert!(port(repo.lease(&ws.id, ACQUIRED + TTL).await).is_none());
    let next = acquire_at(&*repo, &ws.id, "token-2", ACQUIRED + TTL).await;
    assert!(next.fencing > first.fencing);
    assert_lost(repo.set_holds_user_files(&first, true, 2_100).await, &ws.id);
    let unknown = repo
        .acquire_lease(&WorkspaceId::new("nope"), &holder("a1"), "t", 1, TTL)
        .await;
    assert_eq!(
        unknown,
        Err(RepositoryError::WorkspaceNotFound(WorkspaceId::new("nope")))
    );
    let other = create(&*repo, "ws-2").await;
    assert_invalid(
        repo.acquire_lease(&other.id, &holder("a1"), "t", 1, -1)
            .await,
    );
    assert!(port(repo.lease(&other.id, 1).await).is_none());
}

async fn renew_extends_a_current_lease_only<T: WorkspaceRepository>(repo: Arc<T>) {
    let ws = create(&*repo, "ws-1").await;
    let lease = acquire(&*repo, &ws.id).await;
    let renewed = port(repo.renew_lease(&lease, DURING, TTL).await);
    assert_eq!(
        renewed,
        LeaseToken {
            expires_at: DURING + TTL,
            ..lease.clone()
        }
    );
    // The presented token's own `expires_at` is never trusted: the stale
    // copy works because the stored lease is current.
    let later = ACQUIRED + TTL + 200;
    assert!(port(repo.set_holds_user_files(&lease, true, later).await).holds_user_files);
    assert_eq!(
        port(repo.lease(&ws.id, later).await).map(|info| info.expires_at),
        Some(DURING + TTL)
    );
    assert_lost(repo.renew_lease(&lease, DURING + TTL, TTL).await, &ws.id);
    let forged = LeaseToken {
        token: "forged".into(),
        ..lease.clone()
    };
    assert_lost(repo.renew_lease(&forged, DURING, TTL).await, &ws.id);
}

async fn release_drops_only_the_matching_lease<T: WorkspaceRepository>(repo: Arc<T>) {
    let ws = create(&*repo, "ws-1").await;
    let lease = acquire(&*repo, &ws.id).await;
    let wrong = LeaseToken {
        fencing: lease.fencing + 1,
        ..lease.clone()
    };
    assert_lost(repo.release_lease(&wrong).await, &ws.id);
    assert!(port(repo.lease(&ws.id, DURING).await).is_some());
    port(repo.release_lease(&lease).await);
    assert!(port(repo.lease(&ws.id, DURING).await).is_none());
    assert!(port(repo.list_leases().await).is_empty());
    assert_lost(repo.release_lease(&lease).await, &ws.id);
    // An expired lease is still released by its own token.
    let expired = acquire_at(&*repo, &ws.id, "token-2", 5_000).await;
    assert!(port(repo.lease(&ws.id, 9_000).await).is_none());
    port(repo.release_lease(&expired).await);
    assert!(port(repo.list_leases().await).is_empty());
}

async fn fencing_increases_across_acquire_release_and_break<T: WorkspaceRepository>(repo: Arc<T>) {
    let ws = create(&*repo, "ws-1").await;
    let first = acquire(&*repo, &ws.id).await;
    port(repo.release_lease(&first).await);
    let second = acquire_at(&*repo, &ws.id, "token-1", ACQUIRED).await;
    assert!(second.fencing > first.fencing);
    assert!(port(repo.break_lease(&ws.id, second.fencing).await));
    let third = acquire_at(&*repo, &ws.id, "token-1", ACQUIRED).await;
    assert!(third.fencing > second.fencing);
    // Same token text, older fencing numbers: never current again.
    assert_lost(repo.renew_lease(&first, DURING, TTL).await, &ws.id);
    assert_lost(repo.renew_lease(&second, DURING, TTL).await, &ws.id);
    assert_lost(repo.mark_ready(&second, DURING).await, &ws.id);
    port(repo.mark_ready(&third, DURING).await);
}

async fn break_lease_matches_the_fencing_number<T: WorkspaceRepository>(repo: Arc<T>) {
    let ws = create(&*repo, "ws-1").await;
    let lease = acquire(&*repo, &ws.id).await;
    let listed = port(repo.list_leases().await);
    assert_eq!(listed.len(), 1);
    assert_eq!(
        (
            &listed[0].workspace_id,
            listed[0].fencing,
            listed[0].expires_at
        ),
        (&ws.id, lease.fencing, lease.expires_at)
    );
    assert!(!port(repo.break_lease(&ws.id, lease.fencing + 1).await));
    assert!(!port(
        repo.break_lease(&WorkspaceId::new("nope"), lease.fencing)
            .await
    ));
    assert_eq!(port(repo.list_leases().await).len(), 1);
    // Expired or not, recovery may break it.
    assert!(port(repo.break_lease(&ws.id, lease.fencing).await));
    assert!(port(repo.list_leases().await).is_empty());
    assert!(!port(repo.break_lease(&ws.id, lease.fencing).await));
    assert_lost(repo.mark_ready(&lease, DURING).await, &ws.id);
}
