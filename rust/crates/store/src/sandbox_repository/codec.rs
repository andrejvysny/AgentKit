//! Row mapping of the sandbox tables: column lists, enum wire strings and
//! the checked integer conversions between the port's `u64` and SQLite's
//! `i64`.

use super::Work;
use crate::memory_codec::unwire;
use agentkit_workspaces::{
    AgentId, Generation, GrantId, HostPort, LeaseHolder, LeaseInfo, Member, NetworkGrant,
    NetworkRequest, Operation, OperationId, OperationPhase, OperationState, RelPath,
    RepositoryError, RequestId, Sha256, Workspace, WorkspaceId, WorkspaceKey, WorkspaceState,
};
use rusqlite::{Row, types::Type};

pub(super) const WORKSPACE_COLUMNS: &str = "id,kind,key,owner_scope,state,fence_reason,\
generation,image_digest,holds_user_files,created_at,last_used_at,fence_generation";

pub(super) const LEASE_COLUMNS: &str =
    "workspace_id,token,fencing,expires_at,agent_id,run_id,label,host_id,boot_id";

pub(super) const MEMBER_COLUMNS: &str = "workspace_id,agent_id,role,added_at";

pub(super) const OPERATION_COLUMNS: &str = "id,workspace_id,generation,kind,actor_agent_id,\
run_id,phase,receipt,started_at,ended_at,log_path,truncated,image_digest,summary,args_sha256";

pub(super) const GRANT_COLUMNS: &str = "id,workspace_id,host,port,source,granted_at,revoked_at";

pub(super) const REQUEST_COLUMNS: &str =
    "id,workspace_id,host,port,operation_id,first_seen,count,state";

/// A stored value the port cannot represent.
#[derive(Debug, thiserror::Error)]
#[error("unreadable sandbox column value: {0}")]
struct Unreadable(String);

fn unreadable(
    index: usize,
    error: impl std::error::Error + Send + Sync + 'static,
) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(index, Type::Text, Box::new(error))
}

/// A `u64` as a SQLite integer; refused when it does not fit.
pub(super) fn int(value: u64, what: &str) -> Work<i64> {
    i64::try_from(value).map_err(|_| {
        RepositoryError::InvalidInput {
            reason: format!("{what} {value} is too large to store"),
        }
        .into()
    })
}

fn unsigned(row: &Row<'_>, index: usize) -> rusqlite::Result<u64> {
    u64::try_from(row.get::<_, i64>(index)?).map_err(|error| unreadable(index, error))
}

fn host_port(row: &Row<'_>, host: usize, port: usize) -> rusqlite::Result<HostPort> {
    let number =
        u16::try_from(row.get::<_, i64>(port)?).map_err(|error| unreadable(port, error))?;
    HostPort::new(&row.get::<_, String>(host)?, number).map_err(|error| unreadable(host, error))
}

/// The stored `(state, fence_reason, fence_generation)` of a workspace
/// state.
pub(super) fn state_columns(
    state: &WorkspaceState,
) -> Work<(&'static str, Option<&str>, Option<i64>)> {
    Ok(match state {
        WorkspaceState::Ready => ("ready", None, None),
        WorkspaceState::Busy => ("busy", None, None),
        WorkspaceState::Fenced { reason, generation } => (
            "fenced",
            Some(reason),
            Some(int(generation.0, "fence generation")?),
        ),
        WorkspaceState::Deleted => ("deleted", None, None),
    })
}

fn state_from(row: &Row<'_>) -> rusqlite::Result<WorkspaceState> {
    let state: String = row.get(4)?;
    Ok(match state.as_str() {
        "ready" => WorkspaceState::Ready,
        "busy" => WorkspaceState::Busy,
        "fenced" => WorkspaceState::Fenced {
            reason: row.get::<_, Option<String>>(5)?.unwrap_or_default(),
            generation: Generation(unsigned(row, 11)?),
        },
        "deleted" => WorkspaceState::Deleted,
        _ => return Err(unreadable(4, Unreadable(state))),
    })
}

pub(super) fn workspace_from(row: &Row<'_>) -> rusqlite::Result<Workspace> {
    Ok(Workspace {
        id: WorkspaceId::new(row.get::<_, String>(0)?),
        kind: unwire(&row.get::<_, String>(1)?)?,
        key: WorkspaceKey::new(row.get::<_, String>(2)?),
        owner_scope: row.get(3)?,
        state: state_from(row)?,
        generation: Generation(unsigned(row, 6)?),
        image_digest: row.get(7)?,
        holds_user_files: row.get::<_, i64>(8)? != 0,
        created_at: row.get(9)?,
        last_used_at: row.get(10)?,
    })
}

/// A stored lease: its token and the port's view of it.
pub(super) struct StoredLease {
    pub(super) token: String,
    pub(super) info: LeaseInfo,
}

pub(super) fn lease_from(row: &Row<'_>) -> rusqlite::Result<StoredLease> {
    Ok(StoredLease {
        token: row.get(1)?,
        info: LeaseInfo {
            workspace_id: WorkspaceId::new(row.get::<_, String>(0)?),
            holder: LeaseHolder {
                agent_id: AgentId::new(row.get::<_, String>(4)?),
                run_id: row.get(5)?,
                label: row.get(6)?,
                host_id: row.get(7)?,
                boot_id: row.get(8)?,
            },
            fencing: unsigned(row, 2)?,
            expires_at: row.get(3)?,
        },
    })
}

pub(super) fn member_from(row: &Row<'_>) -> rusqlite::Result<Member> {
    Ok(Member {
        workspace_id: WorkspaceId::new(row.get::<_, String>(0)?),
        agent_id: AgentId::new(row.get::<_, String>(1)?),
        role: unwire(&row.get::<_, String>(2)?)?,
        added_at: row.get(3)?,
    })
}

/// The stored `(phase, receipt)` pair: a terminal phase keeps its receipt
/// state as JSON.
pub(super) fn phase_columns(phase: &OperationPhase) -> Work<(&'static str, Option<String>)> {
    Ok(match phase {
        OperationPhase::Dispatching => ("dispatching", None),
        OperationPhase::Running => ("running", None),
        OperationPhase::Terminal(state) => (
            "terminal",
            Some(serde_json::to_string(state).map_err(super::backend)?),
        ),
        OperationPhase::Uncertain => ("uncertain", None),
        OperationPhase::Interrupted(state) => (
            "interrupted",
            Some(serde_json::to_string(state).map_err(super::backend)?),
        ),
    })
}

fn receipt_from(row: &Row<'_>) -> rusqlite::Result<OperationState> {
    let receipt: String = row.get(7)?;
    serde_json::from_str(&receipt).map_err(|error| unreadable(7, error))
}

fn phase_from(row: &Row<'_>) -> rusqlite::Result<OperationPhase> {
    let phase: String = row.get(6)?;
    Ok(match phase.as_str() {
        "dispatching" => OperationPhase::Dispatching,
        "running" => OperationPhase::Running,
        "terminal" => OperationPhase::Terminal(receipt_from(row)?),
        "uncertain" => OperationPhase::Uncertain,
        "interrupted" => OperationPhase::Interrupted(receipt_from(row)?),
        _ => return Err(unreadable(6, Unreadable(phase))),
    })
}

pub(super) fn operation_from(row: &Row<'_>) -> rusqlite::Result<Operation> {
    let log_path = row
        .get::<_, Option<String>>(10)?
        .map(|text| RelPath::parse(&text).map_err(|error| unreadable(10, error)))
        .transpose()?;
    Ok(Operation {
        id: OperationId::new(row.get::<_, String>(0)?),
        workspace_id: WorkspaceId::new(row.get::<_, String>(1)?),
        generation: Generation(unsigned(row, 2)?),
        kind: unwire(&row.get::<_, String>(3)?)?,
        actor_agent_id: AgentId::new(row.get::<_, String>(4)?),
        run_id: row.get(5)?,
        phase: phase_from(row)?,
        started_at: row.get(8)?,
        ended_at: row.get(9)?,
        log_path,
        truncated: row.get::<_, i64>(11)? != 0,
        image_digest: row.get(12)?,
        summary: row.get(13)?,
        args_sha256: Sha256::parse(&row.get::<_, String>(14)?)
            .map_err(|error| unreadable(14, error))?,
    })
}

pub(super) fn grant_from(row: &Row<'_>) -> rusqlite::Result<NetworkGrant> {
    Ok(NetworkGrant {
        id: GrantId::new(row.get::<_, String>(0)?),
        workspace_id: WorkspaceId::new(row.get::<_, String>(1)?),
        target: host_port(row, 2, 3)?,
        source: unwire(&row.get::<_, String>(4)?)?,
        granted_at: row.get(5)?,
        revoked_at: row.get(6)?,
    })
}

pub(super) fn request_from(row: &Row<'_>) -> rusqlite::Result<NetworkRequest> {
    Ok(NetworkRequest {
        id: RequestId::new(row.get::<_, String>(0)?),
        workspace_id: WorkspaceId::new(row.get::<_, String>(1)?),
        target: host_port(row, 2, 3)?,
        operation_id: row.get::<_, Option<String>>(4)?.map(OperationId::new),
        first_seen: row.get(5)?,
        count: unsigned(row, 6)?,
        state: unwire(&row.get::<_, String>(7)?)?,
    })
}
