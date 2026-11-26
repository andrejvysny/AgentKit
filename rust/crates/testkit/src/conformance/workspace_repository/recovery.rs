//! Cases for leaving a fence: the generation a fence records, and the one
//! transaction that moves past it and unfences (ADR 0038 decision 7).

use std::sync::Arc;

use agentkit_workspaces::{
    Generation, OperationPhase, OperationUpdate, RepositoryError, WorkspaceRepository,
    WorkspaceState,
};

use super::super::case_support::port;
use super::states::fenced_at;
use super::{
    ACQUIRED, DURING, TTL, acquire, acquire_at, assert_invalid, assert_lost, assert_state_conflict,
    busy, create,
};

pub(super) async fn fence_records_the_newest_generation_that_may_run<T: WorkspaceRepository>(
    repo: Arc<T>,
) {
    let ws = create(&*repo, "ws-1").await;
    let lease = acquire(&*repo, &ws.id).await;
    // Below the current generation the current one is recorded.
    let low = port(repo.fence(&lease, "low", Generation(0), DURING).await);
    assert_eq!(low.state, fenced_at("low", 1));
    let served = port(repo.fence(&lease, "served", Generation(5), DURING).await);
    assert_eq!(served.state, fenced_at("served", 5));
    // A later fence never lowers it.
    let again = port(repo.fence(&lease, "again", Generation(2), DURING).await);
    assert_eq!(again.state, fenced_at("again", 5));
    // Advancing while fenced keeps the fence and its generation.
    let advanced = port(
        repo.advance_generation(&lease, Generation(1), "sha256:image-2", DURING)
            .await,
    );
    assert_eq!(
        (advanced.generation, advanced.state),
        (Generation(2), fenced_at("again", 5))
    );
    assert_eq!(
        port(repo.workspace(&ws.id).await).map(|ws| ws.state),
        Some(fenced_at("again", 5))
    );
}

pub(super) async fn advance_and_unfence_moves_past_the_fence_at_once<T: WorkspaceRepository>(
    repo: Arc<T>,
) {
    let ws = create(&*repo, "ws-1").await;
    let lease = acquire(&*repo, &ws.id).await;
    port(repo.fence(&lease, "served", Generation(5), DURING).await);
    let stale = repo
        .advance_and_unfence(&lease, Generation(2), Generation(6), "sha256:b", DURING)
        .await;
    assert!(
        matches!(stale, Err(RepositoryError::StaleGeneration { .. })),
        "{stale:?}"
    );
    // `next` must move past the fenced generation.
    assert_invalid(
        repo.advance_and_unfence(&lease, Generation(1), Generation(5), "sha256:b", DURING)
            .await,
    );
    let expired = ACQUIRED + TTL;
    assert_lost(
        repo.advance_and_unfence(&lease, Generation(1), Generation(6), "sha256:b", expired)
            .await,
        &ws.id,
    );
    let ready = port(
        repo.advance_and_unfence(&lease, Generation(1), Generation(6), "sha256:b", DURING)
            .await,
    );
    assert_eq!(
        (ready.generation, ready.state, ready.image_digest.as_str()),
        (Generation(6), WorkspaceState::Ready, "sha256:b")
    );
    // From `Ready` it is a plain advance; never backwards.
    assert_invalid(
        repo.advance_and_unfence(&lease, Generation(6), Generation(6), "sha256:c", DURING)
            .await,
    );
    port(repo.mark_deleted(&lease, DURING).await);
    assert_state_conflict(
        repo.advance_and_unfence(&lease, Generation(6), Generation(7), "sha256:c", DURING)
            .await,
        WorkspaceState::Deleted,
    );
}

pub(super) async fn advance_and_unfence_needs_no_open_operation<T: WorkspaceRepository>(
    repo: Arc<T>,
) {
    let (ws, lease, op) = busy(&*repo, "ws-1").await;
    port(repo.fence(&lease, "watchdog", Generation(1), DURING).await);
    let open = repo
        .advance_and_unfence(&lease, Generation(1), Generation(2), "sha256:b", DURING)
        .await;
    assert!(
        matches!(open, Err(RepositoryError::OperationOpen { .. })),
        "{open:?}"
    );
    // An uncertain operation of the fenced generation does not block it.
    let update = OperationUpdate {
        phase: OperationPhase::Uncertain,
        ended_at: Some(DURING),
        log_path: None,
        truncated: false,
    };
    port(repo.update_operation(&lease, &op.id, &update, DURING).await);
    let ready = port(
        repo.advance_and_unfence(&lease, Generation(1), Generation(2), "sha256:b", DURING)
            .await,
    );
    assert_eq!(ready.state, WorkspaceState::Ready);
    // A busy workspace with an uncertain row becomes ready the same way.
    let other = create(&*repo, "ws-2").await;
    let other_lease = acquire_at(&*repo, &other.id, "token-2", ACQUIRED + TTL / 2).await;
    let moved = port(
        repo.advance_and_unfence(
            &other_lease,
            Generation(1),
            Generation(2),
            "sha256:b",
            DURING,
        )
        .await,
    );
    assert_eq!(
        (moved.generation, moved.state),
        (Generation(2), WorkspaceState::Ready)
    );
    let _ = ws;
}
