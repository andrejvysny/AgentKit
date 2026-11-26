//! Network grants and the denial-to-request flow (owner-side, unfenced).

use super::codec::{GRANT_COLUMNS, REQUEST_COLUMNS, grant_from, int, request_from};
use super::{Work, workspaces};
use crate::memory_codec::wire;
use agentkit_workspaces::{
    DenialRecord, GrantId, NetworkGrant, NetworkRequest, RepositoryError, RequestDecided,
    RequestDecision, RequestId, RequestState, WorkspaceId,
};
use rusqlite::{Connection, OptionalExtension, params};

fn invalid(reason: impl Into<String>) -> RepositoryError {
    RepositoryError::InvalidInput {
        reason: reason.into(),
    }
}

pub(super) fn grant_exists(db: &Connection, id: &GrantId) -> Work<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sandbox_network_grant WHERE id=?)",
        [id.as_str()],
        |row| row.get(0),
    )?)
}

fn grant(db: &Connection, id: &GrantId) -> Work<Option<NetworkGrant>> {
    let sql = format!("SELECT {GRANT_COLUMNS} FROM sandbox_network_grant WHERE id=?");
    Ok(db.query_row(&sql, [id.as_str()], grant_from).optional()?)
}

fn active_for(db: &Connection, grant: &NetworkGrant) -> Work<Option<NetworkGrant>> {
    let sql = format!(
        "SELECT {GRANT_COLUMNS} FROM sandbox_network_grant \
         WHERE workspace_id=? AND host=? AND port=? AND revoked_at IS NULL"
    );
    let args = params![
        grant.workspace_id.as_str(),
        grant.target.host(),
        grant.target.port()
    ];
    Ok(db.query_row(&sql, args, grant_from).optional()?)
}

pub(super) fn insert_grant(db: &Connection, grant: &NetworkGrant) -> Work<NetworkGrant> {
    if !grant.is_active() {
        return Err(invalid("a revoked grant cannot be inserted").into());
    }
    workspaces::require(db, &grant.workspace_id)?;
    if let Some(existing) = active_for(db, grant)? {
        return Ok(existing);
    }
    if grant_exists(db, &grant.id)? {
        return Err(invalid(format!("grant id {} is already used", grant.id)).into());
    }
    db.execute(
        "INSERT INTO sandbox_network_grant(id,workspace_id,host,port,source,granted_at,revoked_at) \
         VALUES(?1,?2,?3,?4,?5,?6,NULL)",
        params![
            grant.id.as_str(),
            grant.workspace_id.as_str(),
            grant.target.host(),
            grant.target.port(),
            wire(&grant.source),
            grant.granted_at
        ],
    )?;
    Ok(grant.clone())
}

pub(super) fn revoke_grant(db: &Connection, id: &GrantId, now: i64) -> Work<NetworkGrant> {
    db.execute(
        "UPDATE sandbox_network_grant SET revoked_at=? WHERE id=? AND revoked_at IS NULL",
        params![now, id.as_str()],
    )?;
    grant(db, id)?.ok_or_else(|| RepositoryError::GrantNotFound(id.clone()).into())
}

pub(super) fn active_grants(
    db: &Connection,
    workspace_id: &WorkspaceId,
) -> Work<Vec<NetworkGrant>> {
    let sql = format!(
        "SELECT {GRANT_COLUMNS} FROM sandbox_network_grant \
         WHERE workspace_id=? AND revoked_at IS NULL ORDER BY granted_at, id"
    );
    let mut statement = db.prepare(&sql)?;
    let rows = statement.query_map([workspace_id.as_str()], grant_from)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn request(db: &Connection, id: &RequestId) -> Work<Option<NetworkRequest>> {
    let sql = format!("SELECT {REQUEST_COLUMNS} FROM sandbox_network_request WHERE id=?");
    Ok(db.query_row(&sql, [id.as_str()], request_from).optional()?)
}

/// The pending or rejected request of a target: at most one exists.
fn open_request(db: &Connection, denial: &DenialRecord) -> Work<Option<NetworkRequest>> {
    let sql = format!(
        "SELECT {REQUEST_COLUMNS} FROM sandbox_network_request \
         WHERE workspace_id=? AND host=? AND port=? AND state<>'approved'"
    );
    let args = params![
        denial.workspace_id.as_str(),
        denial.target.host(),
        denial.target.port()
    ];
    Ok(db.query_row(&sql, args, request_from).optional()?)
}

/// Count the denial on an existing pending or rejected request.
fn count_on(
    db: &Connection,
    mut request: NetworkRequest,
    denial: &DenialRecord,
) -> Work<NetworkRequest> {
    request.count = request
        .count
        .checked_add(denial.attempts)
        .ok_or_else(|| invalid("request count overflow"))?;
    request.operation_id = denial.operation_id.clone().or(request.operation_id);
    db.execute(
        "UPDATE sandbox_network_request SET count=?1, operation_id=?2 WHERE id=?3",
        params![
            int(request.count, "request count")?,
            request.operation_id.as_ref().map(|id| id.as_str()),
            request.id.as_str()
        ],
    )?;
    Ok(request)
}

pub(super) fn record_denial(db: &Connection, denial: &DenialRecord) -> Work<NetworkRequest> {
    if denial.attempts == 0 {
        return Err(invalid("a denial stands for at least one attempt").into());
    }
    workspaces::require(db, &denial.workspace_id)?;
    if let Some(existing) = open_request(db, denial)? {
        return count_on(db, existing, denial);
    }
    if request(db, &denial.id)?.is_some() {
        return Err(invalid(format!("request id {} is already used", denial.id)).into());
    }
    let created = NetworkRequest {
        id: denial.id.clone(),
        workspace_id: denial.workspace_id.clone(),
        target: denial.target.clone(),
        operation_id: denial.operation_id.clone(),
        first_seen: denial.seen_at,
        count: denial.attempts,
        state: RequestState::Pending,
    };
    db.execute(
        "INSERT INTO sandbox_network_request(id,workspace_id,host,port,operation_id,first_seen,count,state) \
         VALUES(?1,?2,?3,?4,?5,?6,?7,'pending')",
        params![
            created.id.as_str(),
            created.workspace_id.as_str(),
            created.target.host(),
            created.target.port(),
            created.operation_id.as_ref().map(|id| id.as_str()),
            created.first_seen,
            int(created.count, "request count")?
        ],
    )?;
    Ok(created)
}

pub(super) fn decide(
    db: &Connection,
    id: &RequestId,
    decision: &RequestDecision,
) -> Work<RequestDecided> {
    let mut request =
        request(db, id)?.ok_or_else(|| RepositoryError::RequestNotFound(id.clone()))?;
    if request.state != RequestState::Pending {
        return Err(RepositoryError::RequestNotPending {
            request_id: id.clone(),
            state: request.state,
        }
        .into());
    }
    let grant = match decision {
        RequestDecision::Reject => {
            request.state = RequestState::Rejected;
            None
        }
        RequestDecision::Approve { grant } => {
            if grant.workspace_id != request.workspace_id || grant.target != request.target {
                return Err(
                    invalid("the grant must name the request's workspace and target").into(),
                );
            }
            request.state = RequestState::Approved;
            Some(insert_grant(db, grant)?)
        }
    };
    db.execute(
        "UPDATE sandbox_network_request SET state=? WHERE id=?",
        params![wire(&request.state), id.as_str()],
    )?;
    Ok(RequestDecided { request, grant })
}

pub(super) fn pending_requests(
    db: &Connection,
    workspace_id: &WorkspaceId,
) -> Work<Vec<NetworkRequest>> {
    let sql = format!(
        "SELECT {REQUEST_COLUMNS} FROM sandbox_network_request \
         WHERE workspace_id=? AND state='pending' ORDER BY first_seen, id"
    );
    let mut statement = db.prepare(&sql)?;
    let rows = statement.query_map([workspace_id.as_str()], request_from)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}
