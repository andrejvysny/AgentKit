//! Durable subagent registry (schema version 10).
//!
//! One row per spawned child chat. The spawn flow registers the child
//! under its `child_chat_id` with the parent binding and the spawn
//! contract; the host projects the child's authoritative turn-task state
//! onto the row (`turn_task_id` names the current turn task and is
//! re-pointed when a follow-up starts a new turn), and the orphan reaper
//! enumerates the non-terminal rows to reap children whose parent died.
//!
//! Lifecycle is exactly the five CHECK-constrained statuses. A terminal
//! row is FROZEN: a later update to a DIFFERENT status is refused with
//! `Ok(false)`, while the same-status update may still fill `result`/
//! `error` — that is what makes result delivery retry idempotent: the
//! first terminal write wins, re-delivery changes nothing.

use crate::{Result, SqliteStore, StoreError};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

/// The complete lifecycle vocabulary of a registry row.
fn valid_status(status: &str) -> bool {
    matches!(
        status,
        "queued" | "running" | "completed" | "failed" | "cancelled"
    )
}

/// The three statuses no row ever leaves.
fn terminal_status(status: &str) -> bool {
    matches!(status, "completed" | "failed" | "cancelled")
}

/// One subagent registry row, in the camelCase wire shape the host and
/// the napi bindings project.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentRecord {
    pub child_chat_id: String,
    pub parent_chat_id: String,
    pub parent_task_id: Option<String>,
    pub role: String,
    pub depth: i64,
    pub prompt: String,
    pub model: Option<String>,
    pub turn_task_id: Option<String>,
    pub status: String,
    pub result: Option<String>,
    pub error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// The spawn payload: everything but the lifecycle state, which starts at
/// `queued` and the timestamps, which both take `now`.
#[derive(Clone, Debug, Default)]
pub struct RegisterSubagent {
    pub child_chat_id: String,
    pub parent_chat_id: String,
    pub parent_task_id: Option<String>,
    pub role: String,
    pub depth: i64,
    pub prompt: String,
    pub model: Option<String>,
    /// A spawn that already knows its first turn task may set it
    /// immediately; `None` leaves it for [`SqliteStore::set_subagent_turn`].
    pub turn_task_id: Option<String>,
    pub now: i64,
}

const SUBAGENT_COLUMNS: &str = "child_chat_id, parent_chat_id, parent_task_id, role, \
                                depth, prompt, model, turn_task_id, status, result, \
                                error, created_at, updated_at";

/// Hard cap on model-authored free text persisted in this table. The
/// parent only ever sees a core-fitted envelope slice; the row exists for
/// audit and replay, so an adversarial parent looping spawns with megabyte
/// prompts/results must not be able to grow the durable store without
/// bound. Truncation is marked, never silent.
const MAX_PERSISTED_CHARS: usize = 256 * 1024;

pub(crate) fn cap_text(text: &str, max_chars: usize) -> String {
    let total = text.chars().count();
    if total <= max_chars {
        return text.to_owned();
    }
    let head: String = text.chars().take(max_chars).collect();
    format!("{head}\n…[truncated {} chars]", total - max_chars)
}

fn read_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SubagentRecord> {
    Ok(SubagentRecord {
        child_chat_id: row.get(0)?,
        parent_chat_id: row.get(1)?,
        parent_task_id: row.get(2)?,
        role: row.get(3)?,
        depth: row.get(4)?,
        prompt: row.get(5)?,
        model: row.get(6)?,
        turn_task_id: row.get(7)?,
        status: row.get(8)?,
        result: row.get(9)?,
        error: row.get(10)?,
        created_at: row.get(11)?,
        updated_at: row.get(12)?,
    })
}

fn select_by_id(db: &Connection, child_chat_id: &str) -> Result<Option<SubagentRecord>> {
    let mut statement = db.prepare(&format!(
        "SELECT {SUBAGENT_COLUMNS} FROM subagents WHERE child_chat_id=?"
    ))?;
    let mut rows = statement.query_map([child_chat_id], read_row)?;
    match rows.next() {
        Some(row) => Ok(Some(row?)),
        None => Ok(None),
    }
}

/// The list scan shared by every ordered read: oldest first, insertion
/// order breaking equal timestamps.
fn list_records(db: &Connection, sql: &str, arg: Option<&str>) -> Result<Vec<SubagentRecord>> {
    let mut statement = db.prepare(sql)?;
    let records = match arg {
        Some(arg) => statement
            .query_map([arg], read_row)?
            .collect::<rusqlite::Result<Vec<_>>>(),
        None => statement
            .query_map([], read_row)?
            .collect::<rusqlite::Result<Vec<_>>>(),
    }?;
    Ok(records)
}

impl SqliteStore {
    /// Register a freshly spawned child. The row starts `queued` with both
    /// timestamps at `now`; a duplicate `child_chat_id` and a negative
    /// depth are invalid inputs, never database errors.
    pub async fn register_subagent(&self, input: RegisterSubagent) -> Result<SubagentRecord> {
        if input.depth < 0 {
            return Err(StoreError::InvalidInput(format!(
                "subagent depth must be >= 0, got {}",
                input.depth
            )));
        }
        // Cap the model-authored prompt at the durable boundary.
        let input = RegisterSubagent {
            prompt: cap_text(&input.prompt, MAX_PERSISTED_CHARS),
            ..input
        };
        self.call(move |db| {
            if select_by_id(db, &input.child_chat_id)?.is_some() {
                return Err(StoreError::InvalidInput(format!(
                    "duplicate subagent: {}",
                    input.child_chat_id
                )));
            }
            db.execute(
                "INSERT INTO subagents(child_chat_id,parent_chat_id,parent_task_id,role,\
                 depth,prompt,model,turn_task_id,status,result,error,created_at,updated_at) \
                 VALUES(?,?,?,?,?,?,?,?,?,NULL,NULL,?,?)",
                params![
                    input.child_chat_id,
                    input.parent_chat_id,
                    input.parent_task_id,
                    input.role,
                    input.depth,
                    input.prompt,
                    input.model,
                    input.turn_task_id,
                    "queued",
                    input.now,
                    input.now,
                ],
            )?;
            select_by_id(db, &input.child_chat_id)?
                .ok_or_else(|| StoreError::NotFound(format!("subagent {}", input.child_chat_id)))
        })
        .await
    }

    /// Project the child-task state onto the row. `false` when the row is
    /// missing; an unknown status is an invalid input. A terminal row is
    /// FROZEN: updating to a DIFFERENT status returns `Ok(false)`, while
    /// the same-status update is accepted so result delivery can retry —
    /// `result`/`error` only ever fill (a set value is never overwritten).
    pub async fn update_subagent(
        &self,
        child_chat_id: &str,
        status: &str,
        result: Option<&str>,
        error: Option<&str>,
        now: i64,
    ) -> Result<bool> {
        if !valid_status(status) {
            return Err(StoreError::InvalidInput(format!(
                "invalid subagent status: {status}"
            )));
        }
        let child_chat_id = child_chat_id.to_owned();
        let status = status.to_owned();
        // Child answers and error strings are model-authored: cap at the
        // durable boundary, marked, never silent.
        let result = result.map(|value| cap_text(value, MAX_PERSISTED_CHARS));
        let error = error.map(|value| cap_text(value, MAX_PERSISTED_CHARS));
        self.call(move |db| {
            let transaction = crate::transaction::atomic(db)?;
            let current: Option<String> = transaction
                .query_row(
                    "SELECT status FROM subagents WHERE child_chat_id=?",
                    [&child_chat_id],
                    |row| row.get(0),
                )
                .ok();
            let Some(current) = current else {
                return Ok(false);
            };
            if terminal_status(&current) && current != status {
                return Ok(false);
            }
            transaction.execute(
                "UPDATE subagents SET status=?, result=COALESCE(result, ?), \
                 error=COALESCE(error, ?), updated_at=? WHERE child_chat_id=?",
                params![status, result, error, now, child_chat_id],
            )?;
            transaction.commit()?;
            Ok(true)
        })
        .await
    }

    /// The only sanctioned terminal → non-terminal transition: the registry
    /// has just bound the row to a provably live chat turn (a queued input
    /// drained into a new turn, or `resume` re-dispatched the assignment),
    /// so the frozen row must rejoin the active ledger — otherwise the live
    /// turn hides from the concurrency caps, the orphan reaper, `await` and
    /// `cancel`. The freeze in `update_subagent` keeps rejecting ad-hoc
    /// status writes; the previous delivery is cleared here because the new
    /// turn owns its own result/error.
    pub async fn reactivate_subagent(
        &self,
        child_chat_id: &str,
        status: &str,
        now: i64,
    ) -> Result<bool> {
        if !matches!(status, "queued" | "running") {
            return Err(StoreError::InvalidInput(format!(
                "cannot reactivate a row to {status}"
            )));
        }
        let child_chat_id = child_chat_id.to_owned();
        let status = status.to_owned();
        self.call(move |db| {
            let updated = db.execute(
                "UPDATE subagents SET status=?, result=NULL, error=NULL, updated_at=? \
                 WHERE child_chat_id=? AND status IN ('completed','failed','cancelled')",
                params![status, now, child_chat_id],
            )?;
            Ok(updated > 0)
        })
        .await
    }

    /// Point the registry row at the child's CURRENT turn task. Overwrite
    /// is deliberate: a follow-up or re-pointed message flow starts a NEW
    /// turn and re-binds tracking to the new task id, so the registry
    /// names the authoritative task for child progress. `false` when the
    /// row is missing.
    pub async fn set_subagent_turn(&self, child_chat_id: &str, turn_task_id: &str) -> Result<bool> {
        let child_chat_id = child_chat_id.to_owned();
        let turn_task_id = turn_task_id.to_owned();
        self.call(move |db| {
            let updated = db.execute(
                "UPDATE subagents SET turn_task_id=? WHERE child_chat_id=?",
                params![turn_task_id, child_chat_id],
            )?;
            Ok(updated > 0)
        })
        .await
    }

    pub async fn get_subagent(&self, child_chat_id: &str) -> Result<Option<SubagentRecord>> {
        let child_chat_id = child_chat_id.to_owned();
        self.call(move |db| select_by_id(db, &child_chat_id)).await
    }

    /// The parent's children, oldest first (insertion order breaking
    /// equal timestamps).
    pub async fn list_subagents(&self, parent_chat_id: &str) -> Result<Vec<SubagentRecord>> {
        let parent_chat_id = parent_chat_id.to_owned();
        self.call(move |db| {
            list_records(
                db,
                &format!(
                    "SELECT {SUBAGENT_COLUMNS} FROM subagents WHERE parent_chat_id=? \
                     ORDER BY created_at ASC, rowid ASC"
                ),
                Some(&parent_chat_id),
            )
        })
        .await
    }

    /// The children spawned under one parent task, oldest first.
    pub async fn list_subagents_by_parent_task(
        &self,
        parent_task_id: &str,
    ) -> Result<Vec<SubagentRecord>> {
        let parent_task_id = parent_task_id.to_owned();
        self.call(move |db| {
            list_records(
                db,
                &format!(
                    "SELECT {SUBAGENT_COLUMNS} FROM subagents WHERE parent_task_id=? \
                     ORDER BY created_at ASC, rowid ASC"
                ),
                Some(&parent_task_id),
            )
        })
        .await
    }

    /// Every non-terminal row, oldest first — the orphan reaper's scan
    /// over children whose parent it cannot name.
    pub async fn list_active_subagents(&self) -> Result<Vec<SubagentRecord>> {
        self.call(move |db| {
            list_records(
                db,
                &format!(
                    "SELECT {SUBAGENT_COLUMNS} FROM subagents \
                     WHERE status IN ('queued','running') ORDER BY created_at ASC, rowid ASC"
                ),
                None,
            )
        })
        .await
    }

    /// In-flight spawn count for the concurrency budget: rows still
    /// `queued` or `running`.
    pub async fn count_active_subagents(&self) -> Result<i64> {
        self.call(move |db| {
            let count = db.query_row(
                "SELECT count(*) FROM subagents WHERE status IN ('queued','running')",
                [],
                |row| row.get::<_, i64>(0),
            )?;
            Ok(count)
        })
        .await
    }
}
