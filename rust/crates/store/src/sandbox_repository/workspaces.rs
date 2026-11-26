//! Workspace records, their state machine and members.

use super::codec::{
    MEMBER_COLUMNS, WORKSPACE_COLUMNS, int, member_from, state_columns, workspace_from,
};
use super::{Work, leases, network};
use crate::memory_codec::wire;
use agentkit_workspaces::{
    AgentId, Generation, Member, NewWorkspace, RepositoryError, Workspace, WorkspaceId,
    WorkspaceKey, WorkspaceState, WorkspaceUpsert,
};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::HashSet;

fn invalid(reason: impl Into<String>) -> RepositoryError {
    RepositoryError::InvalidInput {
        reason: reason.into(),
    }
}

fn conflict(workspace: &Workspace) -> RepositoryError {
    RepositoryError::WorkspaceStateConflict {
        workspace_id: workspace.id.clone(),
        state: workspace.state.clone(),
    }
}

pub(super) fn get(db: &Connection, id: &WorkspaceId) -> Work<Option<Workspace>> {
    let sql = format!("SELECT {WORKSPACE_COLUMNS} FROM sandbox_workspace WHERE id=?");
    Ok(db
        .query_row(&sql, [id.as_str()], workspace_from)
        .optional()?)
}

pub(super) fn require(db: &Connection, id: &WorkspaceId) -> Work<Workspace> {
    get(db, id)?.ok_or_else(|| RepositoryError::WorkspaceNotFound(id.clone()).into())
}

pub(super) fn by_key(db: &Connection, key: &WorkspaceKey) -> Work<Option<Workspace>> {
    let sql = format!(
        "SELECT {WORKSPACE_COLUMNS} FROM sandbox_workspace WHERE key=? AND state<>'deleted'"
    );
    Ok(db
        .query_row(&sql, [key.as_str()], workspace_from)
        .optional()?)
}

pub(super) fn list(db: &Connection) -> Work<Vec<Workspace>> {
    let sql = format!("SELECT {WORKSPACE_COLUMNS} FROM sandbox_workspace ORDER BY created_at, id");
    let mut statement = db.prepare(&sql)?;
    let rows = statement.query_map([], workspace_from)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Refuse a new record whose id or grant ids are taken, or whose grants
/// are not active grants of the new id.
fn check_new(db: &Connection, new: &NewWorkspace) -> Work<()> {
    if get(db, &new.id)?.is_some() {
        return Err(invalid(format!("workspace id {} is already used", new.id)).into());
    }
    let mut ids = HashSet::new();
    for grant in &new.initial_grants {
        if grant.workspace_id != new.id || !grant.is_active() {
            return Err(invalid("initial grants must be active grants of the new id").into());
        }
        if network::grant_exists(db, &grant.id)? || !ids.insert(grant.id.clone()) {
            return Err(invalid(format!("grant id {} is already used", grant.id)).into());
        }
    }
    Ok(())
}

pub(super) fn create_or_get(db: &Connection, new: &NewWorkspace) -> Work<WorkspaceUpsert> {
    if let Some(existing) = by_key(db, &new.key)? {
        return Ok(WorkspaceUpsert {
            workspace: existing,
            created: false,
        });
    }
    check_new(db, new)?;
    db.execute(
        "INSERT INTO sandbox_workspace(id,kind,key,owner_scope,state,fence_reason,generation,\
         image_digest,holds_user_files,created_at,last_used_at,lease_fencing) \
         VALUES(?1,?2,?3,?4,'ready',NULL,?5,?6,0,?7,?7,0)",
        params![
            new.id.as_str(),
            wire(&new.kind),
            new.key.as_str(),
            new.owner_scope,
            int(Generation::FIRST.0, "generation")?,
            new.image_digest,
            new.now
        ],
    )?;
    for grant in &new.initial_grants {
        network::insert_grant(db, grant)?;
    }
    Ok(WorkspaceUpsert {
        workspace: require(db, &new.id)?,
        created: true,
    })
}

/// Write back the mutable fields of a record.
fn save(db: &Connection, workspace: &Workspace) -> Work<Workspace> {
    let (state, reason, fence_generation) = state_columns(&workspace.state)?;
    db.execute(
        "UPDATE sandbox_workspace SET state=?1, fence_reason=?2, generation=?3, image_digest=?4, \
         holds_user_files=?5, last_used_at=?6, fence_generation=?8 WHERE id=?7",
        params![
            state,
            reason,
            int(workspace.generation.0, "generation")?,
            workspace.image_digest,
            workspace.holds_user_files,
            workspace.last_used_at,
            workspace.id.as_str(),
            fence_generation
        ],
    )?;
    Ok(workspace.clone())
}

pub(super) fn touch(db: &Connection, id: &WorkspaceId, now: i64) -> Work<()> {
    let mut workspace = require(db, id)?;
    workspace.last_used_at = workspace.last_used_at.max(now);
    save(db, &workspace)?;
    Ok(())
}

/// Refused with `OperationOpen` while an operation of the workspace is
/// `Dispatching` or `Running`.
fn no_open_operation(db: &Connection, workspace: &Workspace) -> Work<()> {
    match super::operations::oldest_open(db, &workspace.id)? {
        Some(operation_id) => Err(RepositoryError::OperationOpen {
            workspace_id: workspace.id.clone(),
            operation_id,
        }
        .into()),
        None => Ok(()),
    }
}

/// A fenced write of the leased record: `change` gets the record and
/// returns its new value, or a refusal that leaves it untouched.
fn fenced_write(
    db: &Connection,
    lease: &agentkit_workspaces::LeaseToken,
    now: i64,
    change: impl FnOnce(Workspace) -> Work<Workspace>,
) -> Work<Workspace> {
    leases::check(db, lease, now)?;
    let workspace = require(db, &lease.workspace_id)?;
    save(db, &change(workspace)?)
}

type Lease = agentkit_workspaces::LeaseToken;

pub(super) fn set_holds_user_files(
    db: &Connection,
    lease: &Lease,
    value: bool,
    now: i64,
) -> Work<Workspace> {
    fenced_write(db, lease, now, |mut ws| {
        if ws.state == WorkspaceState::Deleted {
            return Err(conflict(&ws).into());
        }
        ws.holds_user_files = value;
        Ok(ws)
    })
}

/// Refused with `OperationUncertain` while an operation of the workspace's
/// current generation is `Uncertain`.
fn no_uncertain_operation(db: &Connection, workspace: &Workspace) -> Work<()> {
    match super::operations::oldest_uncertain(db, &workspace.id, workspace.generation)? {
        Some(operation_id) => Err(RepositoryError::OperationUncertain {
            workspace_id: workspace.id.clone(),
            operation_id,
        }
        .into()),
        None => Ok(()),
    }
}

pub(super) fn mark_ready(db: &Connection, lease: &Lease, now: i64) -> Work<Workspace> {
    fenced_write(db, lease, now, |mut ws| {
        if !matches!(ws.state, WorkspaceState::Busy | WorkspaceState::Ready) {
            return Err(conflict(&ws).into());
        }
        no_open_operation(db, &ws)?;
        no_uncertain_operation(db, &ws)?;
        ws.state = WorkspaceState::Ready;
        Ok(ws)
    })
}

pub(super) fn fence(
    db: &Connection,
    lease: &Lease,
    reason: String,
    generation: Generation,
    now: i64,
) -> Work<Workspace> {
    fenced_write(db, lease, now, |mut ws| {
        let earlier = match &ws.state {
            WorkspaceState::Deleted => return Err(conflict(&ws).into()),
            WorkspaceState::Fenced { generation, .. } => *generation,
            WorkspaceState::Ready | WorkspaceState::Busy => ws.generation,
        };
        ws.state = WorkspaceState::Fenced {
            reason,
            generation: generation.max(earlier).max(ws.generation),
        };
        Ok(ws)
    })
}

/// One transaction: the generation moves to `next` and the workspace is
/// `Ready`, from `Busy`, `Fenced` or `Ready`.
pub(super) fn advance_and_unfence(
    db: &Connection,
    lease: &Lease,
    expected: Generation,
    next: Generation,
    image_digest: String,
    now: i64,
) -> Work<Workspace> {
    fenced_write(db, lease, now, |mut ws| {
        let floor = match &ws.state {
            WorkspaceState::Deleted => return Err(conflict(&ws).into()),
            WorkspaceState::Fenced { generation, .. } => (*generation).max(ws.generation),
            WorkspaceState::Ready | WorkspaceState::Busy => ws.generation,
        };
        if ws.generation != expected {
            return Err(RepositoryError::StaleGeneration {
                workspace_id: ws.id.clone(),
                expected,
                current: ws.generation,
            }
            .into());
        }
        if next <= floor {
            let reason = format!("generation {next} does not move past generation {floor}");
            return Err(invalid(reason).into());
        }
        no_open_operation(db, &ws)?;
        ws.generation = next;
        ws.image_digest = image_digest;
        ws.state = WorkspaceState::Ready;
        Ok(ws)
    })
}

pub(super) fn unfence(db: &Connection, lease: &Lease, now: i64) -> Work<Workspace> {
    fenced_write(db, lease, now, |mut ws| {
        if !matches!(ws.state, WorkspaceState::Fenced { .. }) {
            return Err(conflict(&ws).into());
        }
        no_open_operation(db, &ws)?;
        ws.state = WorkspaceState::Ready;
        Ok(ws)
    })
}

pub(super) fn advance_generation(
    db: &Connection,
    lease: &Lease,
    expected: Generation,
    image_digest: String,
    now: i64,
) -> Work<Workspace> {
    fenced_write(db, lease, now, |mut ws| {
        if ws.state == WorkspaceState::Deleted {
            return Err(conflict(&ws).into());
        }
        if ws.generation != expected {
            return Err(RepositoryError::StaleGeneration {
                workspace_id: ws.id.clone(),
                expected,
                current: ws.generation,
            }
            .into());
        }
        no_open_operation(db, &ws)?;
        ws.generation = ws
            .generation
            .next()
            .ok_or_else(|| invalid("generation overflow"))?;
        ws.image_digest = image_digest;
        if ws.state == WorkspaceState::Busy {
            ws.state = WorkspaceState::Ready;
        }
        Ok(ws)
    })
}

pub(super) fn mark_deleted(db: &Connection, lease: &Lease, now: i64) -> Work<Workspace> {
    fenced_write(db, lease, now, |mut ws| {
        if !matches!(
            ws.state,
            WorkspaceState::Ready | WorkspaceState::Fenced { .. }
        ) {
            return Err(conflict(&ws).into());
        }
        ws.state = WorkspaceState::Deleted;
        Ok(ws)
    })
}

/// `Ready -> Busy` and `last_used_at` for an admitted operation.
pub(super) fn admit(db: &Connection, mut workspace: Workspace, now: i64) -> Work<()> {
    workspace.state = WorkspaceState::Busy;
    workspace.last_used_at = workspace.last_used_at.max(now);
    save(db, &workspace)?;
    Ok(())
}

pub(super) fn member(db: &Connection, id: &WorkspaceId, agent: &AgentId) -> Work<Option<Member>> {
    let sql =
        format!("SELECT {MEMBER_COLUMNS} FROM sandbox_member WHERE workspace_id=? AND agent_id=?");
    Ok(db
        .query_row(&sql, [id.as_str(), agent.as_str()], member_from)
        .optional()?)
}

pub(super) fn add_member(db: &Connection, member: &Member) -> Work<Member> {
    require(db, &member.workspace_id)?;
    db.execute(
        "INSERT INTO sandbox_member(workspace_id,agent_id,role,added_at) VALUES(?,?,?,?) \
         ON CONFLICT(workspace_id,agent_id) DO NOTHING",
        params![
            member.workspace_id.as_str(),
            member.agent_id.as_str(),
            wire(&member.role),
            member.added_at
        ],
    )?;
    self::member(db, &member.workspace_id, &member.agent_id)?
        .ok_or_else(|| super::backend("member row vanished").into())
}

pub(super) fn remove_member(db: &Connection, id: &WorkspaceId, agent: &AgentId) -> Work<bool> {
    let removed = db.execute(
        "DELETE FROM sandbox_member WHERE workspace_id=? AND agent_id=?",
        [id.as_str(), agent.as_str()],
    )?;
    Ok(removed > 0)
}

pub(super) fn list_members(db: &Connection, id: &WorkspaceId) -> Work<Vec<Member>> {
    let sql = format!(
        "SELECT {MEMBER_COLUMNS} FROM sandbox_member WHERE workspace_id=? ORDER BY added_at, agent_id"
    );
    let mut statement = db.prepare(&sql)?;
    let rows = statement.query_map([id.as_str()], member_from)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}
