//! The operation projection: fenced insert and update, and the listings
//! recovery reads.

use super::codec::{OPERATION_COLUMNS, int, operation_from, phase_columns};
use super::{Work, leases, workspaces};
use crate::memory_codec::wire;
use agentkit_workspaces::{
    Generation, LeaseToken, Operation, OperationId, OperationPhase, OperationUpdate,
    RepositoryError, WorkspaceId, WorkspaceState,
};
use rusqlite::{Connection, OptionalExtension, params};

const OPEN: &str = "phase IN ('dispatching','running')";

pub(super) fn get(db: &Connection, id: &OperationId) -> Work<Option<Operation>> {
    let sql = format!("SELECT {OPERATION_COLUMNS} FROM sandbox_operation WHERE id=?");
    Ok(db
        .query_row(&sql, [id.as_str()], operation_from)
        .optional()?)
}

/// The oldest open operation of a workspace.
pub(super) fn oldest_open(
    db: &Connection,
    workspace_id: &WorkspaceId,
) -> Work<Option<OperationId>> {
    let sql = format!(
        "SELECT id FROM sandbox_operation WHERE workspace_id=? AND {OPEN} \
         ORDER BY started_at, id LIMIT 1"
    );
    Ok(db
        .query_row(&sql, [workspace_id.as_str()], |row| row.get::<_, String>(0))
        .optional()?
        .map(OperationId::new))
}

/// The oldest `Uncertain` operation of a workspace's `generation`.
pub(super) fn oldest_uncertain(
    db: &Connection,
    workspace_id: &WorkspaceId,
    generation: Generation,
) -> Work<Option<OperationId>> {
    let sql = "SELECT id FROM sandbox_operation WHERE workspace_id=? AND generation=? \
               AND phase='uncertain' ORDER BY started_at, id LIMIT 1";
    Ok(db
        .query_row(
            sql,
            params![workspace_id.as_str(), int(generation.0, "generation")?],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .map(OperationId::new))
}

fn write_row(db: &Connection, op: &Operation) -> Work<()> {
    let (phase, receipt) = phase_columns(&op.phase)?;
    db.execute(
        "INSERT INTO sandbox_operation(id,workspace_id,generation,kind,actor_agent_id,run_id,phase,\
         receipt,started_at,ended_at,log_path,truncated,image_digest,summary,args_sha256) \
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
        params![
            op.id.as_str(),
            op.workspace_id.as_str(),
            int(op.generation.0, "generation")?,
            wire(&op.kind),
            op.actor_agent_id.as_str(),
            op.run_id,
            phase,
            receipt,
            op.started_at,
            op.ended_at,
            op.log_path.as_ref().map(|path| path.as_str()),
            op.truncated,
            op.image_digest,
            op.summary,
            op.args_sha256.as_str()
        ],
    )?;
    Ok(())
}

/// Checks run in the port's documented order; the first failing one
/// answers.
pub(super) fn insert(db: &Connection, lease: &LeaseToken, op: &Operation, now: i64) -> Work<bool> {
    leases::check(db, lease, now)?;
    if let Some(stored) = get(db, &op.id)? {
        return if stored.same_identity(op) {
            Ok(false)
        } else {
            Err(RepositoryError::OperationConflict(op.id.clone()).into())
        };
    }
    if op.phase != OperationPhase::Dispatching || op.workspace_id != lease.workspace_id {
        return Err(RepositoryError::InvalidInput {
            reason: "a new operation is Dispatching in the leased workspace".into(),
        }
        .into());
    }
    let workspace = workspaces::require(db, &lease.workspace_id)?;
    if workspace.generation != op.generation {
        return Err(RepositoryError::StaleGeneration {
            workspace_id: workspace.id.clone(),
            expected: op.generation,
            current: workspace.generation,
        }
        .into());
    }
    if workspace.state != WorkspaceState::Ready {
        return Err(RepositoryError::WorkspaceStateConflict {
            workspace_id: workspace.id.clone(),
            state: workspace.state,
        }
        .into());
    }
    workspaces::admit(db, workspace, now)?;
    write_row(db, op)?;
    Ok(true)
}

pub(super) fn update(
    db: &Connection,
    lease: &LeaseToken,
    id: &OperationId,
    update: &OperationUpdate,
    now: i64,
) -> Work<Operation> {
    leases::check(db, lease, now)?;
    let mut op = get(db, id)?.ok_or_else(|| RepositoryError::OperationNotFound(id.clone()))?;
    if op.workspace_id != lease.workspace_id {
        return Err(RepositoryError::LeaseLost {
            workspace_id: lease.workspace_id.clone(),
        }
        .into());
    }
    if !op.phase.can_transition_to(&update.phase) {
        return Err(RepositoryError::OperationTransition {
            operation_id: id.clone(),
            from: op.phase,
            to: update.phase,
        }
        .into());
    }
    op.phase = update.phase;
    op.ended_at = update.ended_at.or(op.ended_at);
    op.log_path = update.log_path.clone().or(op.log_path);
    op.truncated |= update.truncated;
    let (phase, receipt) = phase_columns(&op.phase)?;
    db.execute(
        "UPDATE sandbox_operation SET phase=?1, receipt=?2, ended_at=?3, log_path=?4, truncated=?5 \
         WHERE id=?6",
        params![
            phase,
            receipt,
            op.ended_at,
            op.log_path.as_ref().map(|path| path.as_str()),
            op.truncated,
            id.as_str()
        ],
    )?;
    Ok(op)
}

fn query(db: &Connection, sql: &str, args: impl rusqlite::Params) -> Work<Vec<Operation>> {
    let mut statement = db.prepare(sql)?;
    let rows = statement.query_map(args, operation_from)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub(super) fn list(
    db: &Connection,
    workspace_id: &WorkspaceId,
    limit: usize,
) -> Work<Vec<Operation>> {
    let sql = format!(
        "SELECT {OPERATION_COLUMNS} FROM sandbox_operation WHERE workspace_id=? \
         ORDER BY started_at DESC, id DESC LIMIT ?"
    );
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    query(db, &sql, params![workspace_id.as_str(), limit])
}

pub(super) fn list_open(db: &Connection) -> Work<Vec<Operation>> {
    let sql = format!(
        "SELECT {OPERATION_COLUMNS} FROM sandbox_operation WHERE {OPEN} ORDER BY started_at, id"
    );
    query(db, &sql, [])
}
