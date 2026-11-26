//! The per-workspace lease. The fencing number lives on the workspace row
//! (`lease_fencing`, the last one issued), so it keeps increasing after the
//! lease row is released or broken.

use super::codec::{LEASE_COLUMNS, StoredLease, int, lease_from};
use super::{Work, workspaces};
use agentkit_workspaces::{
    LeaseAcquire, LeaseHolder, LeaseInfo, LeaseToken, RepositoryError, WorkspaceId,
};
use rusqlite::{Connection, OptionalExtension, params};

fn invalid(reason: &str) -> RepositoryError {
    RepositoryError::InvalidInput {
        reason: reason.to_owned(),
    }
}

fn lost(workspace_id: &WorkspaceId) -> RepositoryError {
    RepositoryError::LeaseLost {
        workspace_id: workspace_id.clone(),
    }
}

fn expiry(now: i64, ttl_ms: i64) -> Work<i64> {
    if ttl_ms < 0 {
        return Err(invalid("lease TTL must not be negative").into());
    }
    now.checked_add(ttl_ms)
        .ok_or_else(|| invalid("lease expiry overflow").into())
}

fn stored(db: &Connection, workspace_id: &WorkspaceId) -> Work<Option<StoredLease>> {
    let sql = format!("SELECT {LEASE_COLUMNS} FROM sandbox_workspace_lease WHERE workspace_id=?");
    Ok(db
        .query_row(&sql, [workspace_id.as_str()], lease_from)
        .optional()?)
}

/// The token is current: same token and fencing as the stored lease, and
/// the stored lease is unexpired at `now`. Same check as the engine's task
/// fence.
pub(super) fn check(db: &Connection, lease: &LeaseToken, now: i64) -> Work<()> {
    match stored(db, &lease.workspace_id)? {
        Some(stored)
            if stored.token == lease.token
                && stored.info.fencing == lease.fencing
                && stored.info.expires_at > now =>
        {
            Ok(())
        }
        _ => Err(lost(&lease.workspace_id).into()),
    }
}

/// Issue the next fencing number of a workspace.
fn next_fencing(db: &Connection, workspace_id: &WorkspaceId) -> Work<u64> {
    db.execute(
        "UPDATE sandbox_workspace SET lease_fencing=lease_fencing+1 WHERE id=?",
        [workspace_id.as_str()],
    )?;
    let fencing: i64 = db.query_row(
        "SELECT lease_fencing FROM sandbox_workspace WHERE id=?",
        [workspace_id.as_str()],
        |row| row.get(0),
    )?;
    u64::try_from(fencing).map_err(|error| super::backend(error).into())
}

pub(super) fn acquire(
    db: &Connection,
    workspace_id: &WorkspaceId,
    holder: &LeaseHolder,
    token: &str,
    now: i64,
    ttl_ms: i64,
) -> Work<LeaseAcquire> {
    workspaces::require(db, workspace_id)?;
    if let Some(stored) = stored(db, workspace_id)?
        && stored.info.expires_at > now
    {
        return Ok(LeaseAcquire::Held {
            holder: stored.info.holder,
            expires_at: stored.info.expires_at,
        });
    }
    let expires_at = expiry(now, ttl_ms)?;
    let fencing = next_fencing(db, workspace_id)?;
    db.execute(
        "INSERT INTO sandbox_workspace_lease(workspace_id,token,fencing,expires_at,agent_id,run_id,\
         label,host_id,boot_id) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9) ON CONFLICT(workspace_id) DO \
         UPDATE SET token=excluded.token, fencing=excluded.fencing, expires_at=excluded.expires_at, \
         agent_id=excluded.agent_id, run_id=excluded.run_id, label=excluded.label, \
         host_id=excluded.host_id, boot_id=excluded.boot_id",
        params![
            workspace_id.as_str(),
            token,
            int(fencing, "fencing number")?,
            expires_at,
            holder.agent_id.as_str(),
            holder.run_id,
            holder.label,
            holder.host_id,
            holder.boot_id
        ],
    )?;
    Ok(LeaseAcquire::Acquired(LeaseToken {
        workspace_id: workspace_id.clone(),
        token: token.to_owned(),
        fencing,
        expires_at,
    }))
}

pub(super) fn renew(
    db: &Connection,
    lease: &LeaseToken,
    now: i64,
    ttl_ms: i64,
) -> Work<LeaseToken> {
    check(db, lease, now)?;
    let expires_at = expiry(now, ttl_ms)?;
    db.execute(
        "UPDATE sandbox_workspace_lease SET expires_at=? WHERE workspace_id=?",
        params![expires_at, lease.workspace_id.as_str()],
    )?;
    Ok(LeaseToken {
        expires_at,
        ..lease.clone()
    })
}

pub(super) fn release(db: &Connection, lease: &LeaseToken) -> Work<()> {
    let Ok(fencing) = i64::try_from(lease.fencing) else {
        return Err(lost(&lease.workspace_id).into());
    };
    let removed = db.execute(
        "DELETE FROM sandbox_workspace_lease WHERE workspace_id=? AND token=? AND fencing=?",
        params![lease.workspace_id.as_str(), lease.token, fencing],
    )?;
    if removed == 0 {
        return Err(lost(&lease.workspace_id).into());
    }
    Ok(())
}

pub(super) fn current(
    db: &Connection,
    workspace_id: &WorkspaceId,
    now: i64,
) -> Work<Option<LeaseInfo>> {
    Ok(stored(db, workspace_id)?
        .map(|stored| stored.info)
        .filter(|info| info.expires_at > now))
}

pub(super) fn list(db: &Connection) -> Work<Vec<LeaseInfo>> {
    let sql = format!("SELECT {LEASE_COLUMNS} FROM sandbox_workspace_lease ORDER BY workspace_id");
    let mut statement = db.prepare(&sql)?;
    let rows = statement.query_map([], lease_from)?;
    Ok(rows
        .map(|row| row.map(|stored| stored.info))
        .collect::<rusqlite::Result<_>>()?)
}

pub(super) fn break_lease(db: &Connection, workspace_id: &WorkspaceId, fencing: u64) -> Work<bool> {
    let Ok(fencing) = i64::try_from(fencing) else {
        return Ok(false);
    };
    let removed = db.execute(
        "DELETE FROM sandbox_workspace_lease WHERE workspace_id=? AND fencing=?",
        params![workspace_id.as_str(), fencing],
    )?;
    Ok(removed > 0)
}
