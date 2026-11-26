//! Workspace state machine and the fenced-write check.

use std::sync::Arc;

use agentkit_workspaces::{
    Generation, LeaseToken, Operation, OperationId, OperationPhase, OperationState,
    OperationUpdate, RepositoryError, WorkspaceId, WorkspaceRepository, WorkspaceState,
};

use super::{DURING, TTL, acquire, assert_lost, assert_state_conflict, busy, create, port};

fn update(phase: OperationPhase) -> OperationUpdate {
    OperationUpdate {
        phase,
        ended_at: None,
        log_path: None,
        truncated: false,
    }
}

fn succeeded() -> OperationPhase {
    OperationPhase::Terminal(OperationState::Succeeded { exit: 0 })
}

pub(super) async fn settle<T: WorkspaceRepository>(
    repo: &T,
    lease: &LeaseToken,
    op: &OperationId,
) -> Operation {
    port(
        repo.update_operation(lease, op, &update(succeeded()), DURING)
            .await,
    )
}

fn fenced(reason: &str) -> WorkspaceState {
    fenced_at(reason, 1)
}

pub(super) fn fenced_at(reason: &str, generation: u64) -> WorkspaceState {
    WorkspaceState::Fenced {
        reason: reason.to_owned(),
        generation: Generation(generation),
    }
}

fn assert_open<T: std::fmt::Debug>(outcome: Result<T, RepositoryError>, op: &Operation) {
    match outcome {
        Err(RepositoryError::OperationOpen {
            workspace_id,
            operation_id,
        }) => assert_eq!(
            (workspace_id, operation_id),
            (op.workspace_id.clone(), op.id.clone())
        ),
        other => panic!("expected OperationOpen, got {other:?}"),
    }
}

/// Every fenced write presenting `stale` answers `LeaseLost`.
async fn stale_writes<T: WorkspaceRepository>(
    repo: &T,
    stale: &LeaseToken,
    now: i64,
    op: &Operation,
) {
    let ws = &stale.workspace_id;
    assert_lost(repo.set_holds_user_files(stale, true, now).await, ws);
    assert_lost(repo.mark_ready(stale, now).await, ws);
    assert_lost(repo.fence(stale, "stale", Generation::FIRST, now).await, ws);
    assert_lost(repo.unfence(stale, now).await, ws);
    let advance = repo
        .advance_generation(stale, Generation::FIRST, "sha256:image-2", now)
        .await;
    assert_lost(advance, ws);
    assert_lost(repo.mark_deleted(stale, now).await, ws);
    let other = Operation {
        id: OperationId::new("other-op"),
        ..op.clone()
    };
    assert_lost(repo.insert_operation(stale, &other, now).await, ws);
    assert_lost(repo.insert_operation(stale, op, now).await, ws);
    let running = OperationUpdate {
        ended_at: Some(now),
        truncated: true,
        ..update(OperationPhase::Running)
    };
    assert_lost(
        repo.update_operation(stale, &op.id, &running, now).await,
        ws,
    );
    assert_lost(repo.renew_lease(stale, now, TTL).await, ws);
}

pub(super) async fn stale_token_changes_nothing<T: WorkspaceRepository>(repo: Arc<T>) {
    let (ws, lease, op) = busy(&*repo, "ws-1").await;
    let forged = LeaseToken {
        token: "forged".into(),
        ..lease.clone()
    };
    let refenced = LeaseToken {
        fencing: lease.fencing + 1,
        ..lease.clone()
    };
    stale_writes(&*repo, &lease, lease.expires_at, &op).await;
    stale_writes(&*repo, &forged, DURING, &op).await;
    stale_writes(&*repo, &refenced, DURING, &op).await;
    let unknown = LeaseToken {
        workspace_id: WorkspaceId::new("nope"),
        ..lease.clone()
    };
    assert_lost(
        repo.mark_ready(&unknown, DURING).await,
        &unknown.workspace_id,
    );
    assert_eq!(port(repo.workspace(&ws.id).await), Some(ws.clone()));
    assert_eq!(port(repo.operation(&op.id).await), Some(op.clone()));
    assert!(port(repo.operation(&OperationId::new("other-op")).await).is_none());
    let info = port(repo.lease(&ws.id, DURING).await).expect("lease kept");
    assert_eq!(
        (info.fencing, info.expires_at),
        (lease.fencing, lease.expires_at)
    );
}

pub(super) async fn mark_ready_refuses_while_an_operation_is_open<T: WorkspaceRepository>(
    repo: Arc<T>,
) {
    let (ws, lease, op) = busy(&*repo, "ws-1").await;
    assert_eq!(ws.state, WorkspaceState::Busy);
    assert_open(repo.mark_ready(&lease, DURING).await, &op);
    port(
        repo.update_operation(&lease, &op.id, &update(OperationPhase::Running), DURING)
            .await,
    );
    assert_open(repo.mark_ready(&lease, DURING).await, &op);
    let busy_still = port(repo.workspace(&ws.id).await).expect("workspace");
    assert_eq!(busy_still.state, WorkspaceState::Busy);
    // An uncertain operation of the current generation blocks too: the
    // workspace stays Busy until the generation advances past it.
    port(
        repo.update_operation(&lease, &op.id, &update(OperationPhase::Uncertain), DURING)
            .await,
    );
    assert_uncertain(repo.mark_ready(&lease, DURING).await, &op);
    let advanced = port(
        repo.advance_generation(&lease, ws.generation, "sha256:image-2", DURING)
            .await,
    );
    assert_eq!(
        (advanced.state, advanced.generation),
        (WorkspaceState::Ready, Generation(2))
    );
    let again = port(repo.mark_ready(&lease, DURING).await);
    assert_eq!(again.state, WorkspaceState::Ready);
    assert_eq!(again.generation, Generation(2));
}

fn assert_uncertain<T: std::fmt::Debug>(outcome: Result<T, RepositoryError>, op: &Operation) {
    match outcome {
        Err(RepositoryError::OperationUncertain {
            workspace_id,
            operation_id,
        }) => assert_eq!(
            (workspace_id, operation_id),
            (op.workspace_id.clone(), op.id.clone())
        ),
        other => panic!("expected OperationUncertain, got {other:?}"),
    }
}

async fn insert<T: WorkspaceRepository>(
    repo: &T,
    lease: &LeaseToken,
    id: &str,
    at: i64,
) -> Operation {
    let ws = port(repo.workspace(&lease.workspace_id).await).expect("workspace");
    let op = super::dispatching(id, &ws, at);
    assert!(port(repo.insert_operation(lease, &op, DURING).await));
    op
}

async fn unsure<T: WorkspaceRepository>(repo: &T, lease: &LeaseToken, op: &Operation) {
    let to = update(OperationPhase::Uncertain);
    port(repo.update_operation(lease, &op.id, &to, DURING).await);
}

/// The rule behind "uncertain -> stop -> proven -> ready at generation + 1":
/// `mark_ready` refuses while the current generation has an uncertain
/// operation, on `Busy` and on `Ready` alike; a late receipt or an
/// advanced generation lifts it; an open operation answers first.
pub(super) async fn mark_ready_refuses_while_the_current_generation_is_uncertain<
    T: WorkspaceRepository,
>(
    repo: Arc<T>,
) {
    let (ws, lease, first) = busy(&*repo, "ws-1").await;
    unsure(&*repo, &lease, &first).await;
    assert_uncertain(repo.mark_ready(&lease, DURING).await, &first);
    // `unfence` is not refused by it; a `Ready` workspace still is.
    port(
        repo.fence(&lease, "stop not proven", Generation::FIRST, DURING)
            .await,
    );
    assert_eq!(
        port(repo.unfence(&lease, DURING).await).state,
        WorkspaceState::Ready
    );
    assert_uncertain(repo.mark_ready(&lease, DURING).await, &first);
    let kept = port(repo.workspace(&ws.id).await).expect("workspace");
    assert_eq!(kept.state, WorkspaceState::Ready);
    // A late receipt settles it and lifts the refusal.
    settle(&*repo, &lease, &first.id).await;
    port(repo.mark_ready(&lease, DURING).await);
    // Two uncertain operations: the oldest is named.
    let older = insert(&*repo, &lease, "op-older", 2_000).await;
    unsure(&*repo, &lease, &older).await;
    port(repo.fence(&lease, "check", Generation::FIRST, DURING).await);
    port(repo.unfence(&lease, DURING).await);
    let newer = insert(&*repo, &lease, "op-newer", 3_000).await;
    // An open operation answers before an uncertain one.
    assert_open(repo.mark_ready(&lease, DURING).await, &newer);
    unsure(&*repo, &lease, &newer).await;
    assert_uncertain(repo.mark_ready(&lease, DURING).await, &older);
    // Advancing is allowed with uncertain operations; the older
    // generation's ones no longer block.
    let advanced = port(
        repo.advance_generation(&lease, Generation::FIRST, "sha256:image-2", DURING)
            .await,
    );
    assert_eq!(
        (advanced.state, advanced.generation),
        (WorkspaceState::Ready, Generation(2))
    );
    assert_eq!(
        port(repo.mark_ready(&lease, DURING).await).generation,
        Generation(2)
    );
}

pub(super) async fn open_operation_blocks_unfence_and_advance<T: WorkspaceRepository>(
    repo: Arc<T>,
) {
    let (ws, lease, op) = busy(&*repo, "ws-1").await;
    assert_eq!(
        port(
            repo.fence(&lease, "receipt unreadable", Generation::FIRST, DURING)
                .await
        )
        .state,
        fenced("receipt unreadable")
    );
    assert_open(repo.unfence(&lease, DURING).await, &op);
    assert_open(
        repo.advance_generation(&lease, ws.generation, "sha256:image-2", DURING)
            .await,
        &op,
    );
    // A stale expectation answers before the open operation.
    let stale = repo
        .advance_generation(&lease, Generation(5), "sha256:image-2", DURING)
        .await;
    assert!(
        matches!(stale, Err(RepositoryError::StaleGeneration { .. })),
        "{stale:?}"
    );
    settle(&*repo, &lease, &op.id).await;
    assert_eq!(
        port(repo.unfence(&lease, DURING).await).state,
        WorkspaceState::Ready
    );
}

pub(super) async fn fence_and_unfence_round_trip<T: WorkspaceRepository>(repo: Arc<T>) {
    let ws = create(&*repo, "ws-1").await;
    let lease = acquire(&*repo, &ws.id).await;
    let first = port(
        repo.fence(&lease, "stop not proven", Generation::FIRST, DURING)
            .await,
    );
    assert_eq!(first.state, fenced("stop not proven"));
    let second = port(
        repo.fence(&lease, "still unproven", Generation::FIRST, DURING)
            .await,
    );
    assert_eq!(second.state, fenced("still unproven"));
    assert_state_conflict(
        repo.mark_ready(&lease, DURING).await,
        fenced("still unproven"),
    );
    let ready = port(repo.unfence(&lease, DURING).await);
    assert_eq!(ready.state, WorkspaceState::Ready);
    assert_state_conflict(repo.unfence(&lease, DURING).await, WorkspaceState::Ready);
    port(repo.mark_deleted(&lease, DURING).await);
    assert_state_conflict(
        repo.fence(&lease, "late", Generation::FIRST, DURING).await,
        WorkspaceState::Deleted,
    );
    assert_state_conflict(repo.unfence(&lease, DURING).await, WorkspaceState::Deleted);
    assert_state_conflict(
        repo.mark_ready(&lease, DURING).await,
        WorkspaceState::Deleted,
    );
    // A busy workspace can be fenced too.
    let (busy_ws, busy_lease, _) = busy(&*repo, "ws-2").await;
    let fenced_busy = port(
        repo.fence(&busy_lease, "watchdog", Generation::FIRST, DURING)
            .await,
    );
    assert_eq!(
        (fenced_busy.id, fenced_busy.state),
        (busy_ws.id, fenced("watchdog"))
    );
}

pub(super) async fn advance_generation_compares_and_swaps<T: WorkspaceRepository>(repo: Arc<T>) {
    let (ws, lease, op) = busy(&*repo, "ws-1").await;
    settle(&*repo, &lease, &op.id).await;
    let stale = repo
        .advance_generation(&lease, Generation(2), "sha256:image-2", DURING)
        .await;
    assert_eq!(
        stale,
        Err(RepositoryError::StaleGeneration {
            workspace_id: ws.id.clone(),
            expected: Generation(2),
            current: Generation::FIRST,
        })
    );
    assert_eq!(port(repo.workspace(&ws.id).await), Some(ws.clone()));
    let advanced = port(
        repo.advance_generation(&lease, Generation::FIRST, "sha256:image-2", DURING)
            .await,
    );
    assert_eq!(
        (advanced.generation, advanced.image_digest.as_str()),
        (Generation(2), "sha256:image-2")
    );
    assert_eq!(advanced.state, WorkspaceState::Ready);
    let again = repo
        .advance_generation(&lease, Generation::FIRST, "sha256:image-3", DURING)
        .await;
    assert!(
        matches!(
            again,
            Err(RepositoryError::StaleGeneration {
                current: Generation(2),
                ..
            })
        ),
        "{again:?}"
    );
    port(
        repo.fence(&lease, "recovering", Generation::FIRST, DURING)
            .await,
    );
    let fenced_advance = port(
        repo.advance_generation(&lease, Generation(2), "sha256:image-3", DURING)
            .await,
    );
    assert_eq!(fenced_advance.generation, Generation(3));
    assert_eq!(fenced_advance.state, fenced_at("recovering", 2));
    port(repo.mark_deleted(&lease, DURING).await);
    let deleted = repo
        .advance_generation(&lease, Generation(3), "sha256:image-4", DURING)
        .await;
    assert_state_conflict(deleted, WorkspaceState::Deleted);
}

pub(super) async fn mark_deleted_needs_ready_or_fenced<T: WorkspaceRepository>(repo: Arc<T>) {
    let (ws, lease, op) = busy(&*repo, "ws-1").await;
    assert_state_conflict(
        repo.mark_deleted(&lease, DURING).await,
        WorkspaceState::Busy,
    );
    settle(&*repo, &lease, &op.id).await;
    port(repo.mark_ready(&lease, DURING).await);
    let deleted = port(repo.mark_deleted(&lease, DURING).await);
    assert_eq!(
        (deleted.id, deleted.state),
        (ws.id, WorkspaceState::Deleted)
    );
    assert_state_conflict(
        repo.mark_deleted(&lease, DURING).await,
        WorkspaceState::Deleted,
    );
    let other = create(&*repo, "ws-2").await;
    let other_lease = acquire(&*repo, &other.id).await;
    port(
        repo.fence(&other_lease, "stop proven later", Generation::FIRST, DURING)
            .await,
    );
    let gone = port(repo.mark_deleted(&other_lease, DURING).await);
    assert_eq!(gone.state, WorkspaceState::Deleted);
}

pub(super) async fn holds_user_files_is_refused_on_deleted<T: WorkspaceRepository>(repo: Arc<T>) {
    let ws = create(&*repo, "ws-1").await;
    let lease = acquire(&*repo, &ws.id).await;
    assert!(port(repo.set_holds_user_files(&lease, true, DURING).await).holds_user_files);
    let stored = port(repo.workspace(&ws.id).await).expect("workspace");
    assert!(stored.holds_user_files);
    assert_eq!(stored.last_used_at, ws.last_used_at);
    port(repo.fence(&lease, "check", Generation::FIRST, DURING).await);
    assert!(!port(repo.set_holds_user_files(&lease, false, DURING).await).holds_user_files);
    assert!(port(repo.set_holds_user_files(&lease, true, DURING).await).holds_user_files);
    port(repo.mark_deleted(&lease, DURING).await);
    assert_state_conflict(
        repo.set_holds_user_files(&lease, false, DURING).await,
        WorkspaceState::Deleted,
    );
    let kept = port(repo.workspace(&ws.id).await).expect("workspace");
    assert!(kept.holds_user_files);
}
