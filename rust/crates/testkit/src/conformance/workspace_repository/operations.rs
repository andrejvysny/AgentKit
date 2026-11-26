//! The operation projection: insert order, replay, phase transitions and
//! the listings recovery reads.

use std::sync::Arc;

use agentkit_workspaces::{
    AgentId, Generation, LeaseToken, Operation, OperationId, OperationKindTag, OperationPhase,
    OperationState, OperationUpdate, RelPath, RepositoryError, Sha256, WorkspaceId,
    WorkspaceRepository, WorkspaceState,
};

use super::states::settle;
use super::{
    DURING, acquire, assert_invalid, assert_lost, assert_state_conflict, busy, create, dispatching,
    port,
};

fn to(phase: OperationPhase) -> OperationUpdate {
    OperationUpdate {
        phase,
        ended_at: None,
        log_path: None,
        truncated: false,
    }
}

fn terminal(state: OperationState) -> OperationPhase {
    OperationPhase::Terminal(state)
}

fn interrupted() -> OperationPhase {
    OperationPhase::Interrupted(OperationState::Cancelled)
}

async fn moved<T: WorkspaceRepository>(
    repo: &T,
    lease: &LeaseToken,
    op: &OperationId,
    phase: OperationPhase,
) -> Operation {
    port(repo.update_operation(lease, op, &to(phase), DURING).await)
}

async fn refused<T: WorkspaceRepository>(
    repo: &T,
    lease: &LeaseToken,
    op: &Operation,
    phase: OperationPhase,
) {
    let outcome = repo
        .update_operation(lease, &op.id, &to(phase), DURING)
        .await;
    let stored = port(repo.operation(&op.id).await).expect("operation");
    assert_eq!(
        outcome,
        Err(RepositoryError::OperationTransition {
            operation_id: op.id.clone(),
            from: stored.phase,
            to: phase,
        })
    );
}

/// Insert `id` into a `Ready` workspace.
async fn start<T: WorkspaceRepository>(
    repo: &T,
    lease: &LeaseToken,
    id: &str,
    started_at: i64,
) -> Operation {
    let ws = port(repo.workspace(&lease.workspace_id).await).expect("workspace");
    let op = dispatching(id, &ws, started_at);
    assert!(port(repo.insert_operation(lease, &op, DURING).await));
    op
}

pub(super) async fn insert_operation_turns_ready_into_busy<T: WorkspaceRepository>(repo: Arc<T>) {
    let ws = create(&*repo, "ws-1").await;
    let lease = acquire(&*repo, &ws.id).await;
    let op = dispatching("op-1", &ws, 400);
    assert!(port(repo.insert_operation(&lease, &op, DURING).await));
    let stored = port(repo.workspace(&ws.id).await).expect("workspace");
    assert_eq!(
        (stored.state, stored.last_used_at),
        (WorkspaceState::Busy, DURING)
    );
    assert_eq!(port(repo.operation(&op.id).await), Some(op.clone()));
    assert_eq!(port(repo.list_open_operations().await), vec![op]);
    // `last_used_at` never moves back.
    let later = create(&*repo, "ws-2").await;
    port(repo.touch_workspace(&later.id, 9_000).await);
    let later_lease = acquire(&*repo, &later.id).await;
    let late = dispatching("op-2", &later, 400);
    assert!(port(
        repo.insert_operation(&later_lease, &late, DURING).await
    ));
    let kept = port(repo.workspace(&later.id).await).expect("workspace");
    assert_eq!(kept.last_used_at, 9_000);
}

/// `op` with one identity field changed, each.
fn identity_variants(op: &Operation) -> Vec<Operation> {
    vec![
        Operation {
            summary: "rm -rf build".into(),
            ..op.clone()
        },
        Operation {
            run_id: "run-other".into(),
            ..op.clone()
        },
        Operation {
            generation: Generation(2),
            ..op.clone()
        },
        Operation {
            actor_agent_id: AgentId::new("a2"),
            ..op.clone()
        },
        Operation {
            kind: OperationKindTag::WriteFile,
            ..op.clone()
        },
        Operation {
            args_sha256: Sha256::of(b"other arguments"),
            ..op.clone()
        },
    ]
}

pub(super) async fn insert_operation_replays_and_conflicts<T: WorkspaceRepository>(repo: Arc<T>) {
    let (ws, lease, op) = busy(&*repo, "ws-1").await;
    // Times, phase, digest and output fields are not identity.
    let replay = Operation {
        started_at: 9,
        image_digest: "sha256:other".into(),
        truncated: true,
        ..op.clone()
    };
    assert!(!port(repo.insert_operation(&lease, &replay, DURING).await));
    assert_eq!(port(repo.operation(&op.id).await), Some(op.clone()));
    let variants = identity_variants(&op);
    for variant in &variants {
        let outcome = repo.insert_operation(&lease, variant, DURING).await;
        assert_eq!(
            outcome,
            Err(RepositoryError::OperationConflict(op.id.clone()))
        );
    }
    // A settled operation still replays as `Ok(false)`.
    settle(&*repo, &lease, &op.id).await;
    assert!(!port(repo.insert_operation(&lease, &op, DURING).await));
    let stored = port(repo.workspace(&ws.id).await).expect("workspace");
    assert_eq!(stored.state, WorkspaceState::Busy);
    assert_eq!(port(repo.list_operations(&ws.id, 10).await).len(), 1);
}

pub(super) async fn insert_operation_checks_in_documented_order<T: WorkspaceRepository>(
    repo: Arc<T>,
) {
    let ws = create(&*repo, "ws-1").await;
    let lease = acquire(&*repo, &ws.id).await;
    let running = Operation {
        phase: OperationPhase::Running,
        ..dispatching("op-a", &ws, 1)
    };
    assert_invalid(repo.insert_operation(&lease, &running, DURING).await);
    let other = create(&*repo, "ws-2").await;
    let foreign = dispatching("op-b", &other, 1);
    assert_invalid(repo.insert_operation(&lease, &foreign, DURING).await);
    // On a fenced workspace a stale generation answers first.
    port(repo.fence(&lease, "check", Generation::FIRST, DURING).await);
    let stale = Operation {
        generation: Generation(7),
        ..dispatching("op-c", &ws, 1)
    };
    assert_eq!(
        repo.insert_operation(&lease, &stale, DURING).await,
        Err(RepositoryError::StaleGeneration {
            workspace_id: ws.id.clone(),
            expected: Generation(7),
            current: Generation::FIRST,
        })
    );
    let fenced_insert = repo
        .insert_operation(&lease, &dispatching("op-d", &ws, 1), DURING)
        .await;
    assert_state_conflict(
        fenced_insert,
        WorkspaceState::Fenced {
            reason: "check".into(),
            generation: Generation::FIRST,
        },
    );
    for id in ["op-a", "op-b", "op-c", "op-d"] {
        assert!(port(repo.operation(&OperationId::new(id)).await).is_none());
    }
}

pub(super) async fn insert_operation_needs_a_ready_workspace<T: WorkspaceRepository>(repo: Arc<T>) {
    let ws = create(&*repo, "ws-1").await;
    let lease = acquire(&*repo, &ws.id).await;
    let admitted = start(&*repo, &lease, "op-e", 2).await;
    let second = repo
        .insert_operation(&lease, &dispatching("op-f", &ws, 3), DURING)
        .await;
    assert_state_conflict(second, WorkspaceState::Busy);
    // An existing id answers before the input checks.
    let clash = Operation {
        phase: OperationPhase::Running,
        summary: "other".into(),
        ..admitted.clone()
    };
    let outcome = repo.insert_operation(&lease, &clash, DURING).await;
    assert_eq!(
        outcome,
        Err(RepositoryError::OperationConflict(admitted.id.clone()))
    );
    assert!(port(repo.operation(&OperationId::new("op-f")).await).is_none());
    settle(&*repo, &lease, &admitted.id).await;
    port(repo.mark_ready(&lease, DURING).await);
    port(repo.mark_deleted(&lease, DURING).await);
    let deleted = repo
        .insert_operation(&lease, &dispatching("op-g", &ws, 4), DURING)
        .await;
    assert_state_conflict(deleted, WorkspaceState::Deleted);
}

pub(super) async fn update_operation_follows_phase_rules<T: WorkspaceRepository>(repo: Arc<T>) {
    let (ws, lease, op) = busy(&*repo, "ws-1").await;
    refused(&*repo, &lease, &op, OperationPhase::Dispatching).await;
    moved(&*repo, &lease, &op.id, OperationPhase::Running).await;
    moved(&*repo, &lease, &op.id, OperationPhase::Running).await;
    refused(&*repo, &lease, &op, terminal(OperationState::Running)).await;
    let done = terminal(OperationState::Succeeded { exit: 0 });
    assert_eq!(moved(&*repo, &lease, &op.id, done).await.phase, done);
    // Settled once (INV-7).
    let failed = terminal(OperationState::Failed {
        exit: Some(1),
        signal: None,
    });
    refused(&*repo, &lease, &op, failed).await;
    refused(&*repo, &lease, &op, interrupted()).await;
    refused(&*repo, &lease, &op, OperationPhase::Uncertain).await;
    // Updates never change the workspace state.
    let stored = port(repo.workspace(&ws.id).await).expect("workspace");
    assert_eq!(stored.state, WorkspaceState::Busy);
    port(repo.mark_ready(&lease, DURING).await);
    let unsure = start(&*repo, &lease, "op-2", 2_000).await;
    moved(&*repo, &lease, &unsure.id, OperationPhase::Uncertain).await;
    refused(&*repo, &lease, &unsure, OperationPhase::Running).await;
    refused(&*repo, &lease, &unsure, OperationPhase::Uncertain).await;
    moved(&*repo, &lease, &unsure.id, interrupted()).await;
    refused(&*repo, &lease, &unsure, done).await;
    port(repo.mark_ready(&lease, DURING).await);
    let late = start(&*repo, &lease, "op-3", 3_000).await;
    moved(&*repo, &lease, &late.id, OperationPhase::Uncertain).await;
    let receipt = terminal(OperationState::TimedOut);
    assert_eq!(
        moved(&*repo, &lease, &late.id, receipt).await.phase,
        receipt
    );
    port(repo.mark_ready(&lease, DURING).await);
    let direct = start(&*repo, &lease, "op-4", 4_000).await;
    let cancelled = terminal(OperationState::Cancelled);
    assert_eq!(
        moved(&*repo, &lease, &direct.id, cancelled).await.phase,
        cancelled
    );
}

pub(super) async fn update_operation_keeps_unset_fields<T: WorkspaceRepository>(repo: Arc<T>) {
    let (_, lease, op) = busy(&*repo, "ws-1").await;
    let log = RelPath::parse(".sandbox/logs/op-1.log").expect("log path");
    let running = OperationUpdate {
        phase: OperationPhase::Running,
        ended_at: None,
        log_path: Some(log.clone()),
        truncated: true,
    };
    let first = port(
        repo.update_operation(&lease, &op.id, &running, DURING)
            .await,
    );
    assert_eq!(
        (first.ended_at, first.log_path.clone(), first.truncated),
        (None, Some(log.clone()), true)
    );
    let settled = OperationUpdate {
        phase: OperationPhase::Terminal(OperationState::Failed {
            exit: None,
            signal: Some(9),
        }),
        ended_at: Some(2_000),
        log_path: None,
        truncated: false,
    };
    let last = port(
        repo.update_operation(&lease, &op.id, &settled, DURING)
            .await,
    );
    assert_eq!(
        (last.ended_at, last.log_path.clone(), last.truncated),
        (Some(2_000), Some(log), true)
    );
    assert_eq!(last.phase, settled.phase);
    assert_eq!(
        (last.started_at, &last.summary),
        (op.started_at, &op.summary)
    );
    assert_eq!(port(repo.operation(&op.id).await), Some(last));
}

pub(super) async fn update_operation_checks_lease_then_row<T: WorkspaceRepository>(repo: Arc<T>) {
    let (ws, lease, _) = busy(&*repo, "ws-1").await;
    let (_, _, other_op) = busy(&*repo, "ws-2").await;
    let stale = LeaseToken {
        token: "forged".into(),
        ..lease.clone()
    };
    let unknown = OperationId::new("nope");
    let running = to(OperationPhase::Running);
    assert_lost(
        repo.update_operation(&stale, &unknown, &running, DURING)
            .await,
        &ws.id,
    );
    assert_eq!(
        repo.update_operation(&lease, &unknown, &running, DURING)
            .await,
        Err(RepositoryError::OperationNotFound(unknown))
    );
    // A current lease of another workspace is not the operation's lease.
    let crossed = repo
        .update_operation(&lease, &other_op.id, &running, DURING)
        .await;
    assert_lost(crossed, &ws.id);
    assert_eq!(port(repo.operation(&other_op.id).await), Some(other_op));
}

pub(super) async fn operations_list_newest_first_and_open_only<T: WorkspaceRepository>(
    repo: Arc<T>,
) {
    let ws = create(&*repo, "ws-1").await;
    let lease = acquire(&*repo, &ws.id).await;
    let a = start(&*repo, &lease, "op-a", 10).await;
    settle(&*repo, &lease, &a.id).await;
    port(repo.mark_ready(&lease, DURING).await);
    let b = start(&*repo, &lease, "op-b", 30).await;
    moved(&*repo, &lease, &b.id, OperationPhase::Uncertain).await;
    // An uncertain operation keeps the workspace Busy until the generation
    // advances past it.
    port(
        repo.advance_generation(&lease, ws.generation, "sha256:image-2", DURING)
            .await,
    );
    let d = start(&*repo, &lease, "op-d", 30).await;
    settle(&*repo, &lease, &d.id).await;
    port(repo.mark_ready(&lease, DURING).await);
    let c = start(&*repo, &lease, "op-c", 20).await;
    let c = moved(&*repo, &lease, &c.id, OperationPhase::Running).await;
    let ids = |ops: Vec<Operation>| -> Vec<String> {
        ops.into_iter()
            .map(|op| op.id.as_str().to_owned())
            .collect()
    };
    assert_eq!(
        ids(port(repo.list_operations(&ws.id, 10).await)),
        ["op-d", "op-b", "op-c", "op-a"]
    );
    assert_eq!(
        ids(port(repo.list_operations(&ws.id, 2).await)),
        ["op-d", "op-b"]
    );
    assert!(port(repo.list_operations(&ws.id, 0).await).is_empty());
    let nowhere = WorkspaceId::new("nope");
    assert!(port(repo.list_operations(&nowhere, 10).await).is_empty());
    let (_, _, other) = busy(&*repo, "ws-2").await;
    assert_eq!(port(repo.list_open_operations().await), vec![c, other]);
}
