//! Durable advisor findings (schema version 10).
//!
//! One row per advice observation: the explicit flow records what the
//! user asked the advisor to inspect, the watchdog records what it noticed
//! unprompted. The source and severity CHECKs pin the vocabularies the
//! host protocol speaks; `detail` is a JSON object the finding UI renders
//! (non-objects are invalid input at write time, never a stored string
//! that renders as noise). `escalated` marks findings the host already
//! surfaced to the user, and `count_advisor_findings_since` is the
//! watchdog rate-limit window: the host refuses a new unprompted sweep
//! while too many findings of the window already stand.
//!
//! Inserts are idempotent on `finding_id`: a retried report of the same
//! finding returns the existing row unchanged, never a duplicate and
//! never an error.

/// Cap a finding title (display string) at the durable boundary.
const MAX_TITLE_CHARS: usize = 4_000;
/// Cap each model-authored string inside a finding's evidence object.
const MAX_DETAIL_STRING_CHARS: usize = 16_000;
/// Cap each model-authored ARRAY inside a finding's evidence object: string
/// lengths alone do not bound a payload of ten thousand one-character
/// items, and the array length is model-controlled just as the strings are.
const MAX_DETAIL_ARRAY_ITEMS: usize = 256;

/// Recursively cap model-authored strings in an evidence value. Quotes of
/// workspace content are the whole point of `detail`, but their length is
/// model- (and workspace-)controlled: bound each string, keep the shape.
fn cap_detail(value: &mut Value, max_chars: usize) {
    match value {
        Value::String(text) => {
            *text = crate::subagents::cap_text(text, max_chars);
        }
        Value::Array(items) => {
            if items.len() > MAX_DETAIL_ARRAY_ITEMS {
                items.truncate(MAX_DETAIL_ARRAY_ITEMS);
            }
            for item in items {
                cap_detail(item, max_chars);
            }
        }
        Value::Object(entries) => {
            for item in entries.values_mut() {
                cap_detail(item, max_chars);
            }
        }
        _ => {}
    }
}

use crate::{Result, SqliteStore, StoreError};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The two advice sources the protocol knows.
fn valid_source(source: &str) -> bool {
    matches!(source, "explicit" | "watchdog")
}

/// The severity ladder the host routes on.
fn valid_severity(severity: &str) -> bool {
    matches!(severity, "info" | "warning" | "critical")
}

/// One advisor finding, in the camelCase wire shape the host and the napi
/// bindings project. `detail` is always a JSON object.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdvisorFindingRecord {
    pub finding_id: String,
    pub chat_id: String,
    pub source: String,
    pub trigger_kind: String,
    pub severity: String,
    pub title: String,
    pub detail: Value,
    pub model: String,
    pub created_at: i64,
    pub escalated: bool,
}

/// The report payload: everything but the stored timestamp and the
/// escalation flag, which start at `now` and unset.
#[derive(Clone, Debug, Default)]
pub struct InsertFinding {
    pub finding_id: String,
    pub chat_id: String,
    pub source: String,
    pub trigger_kind: String,
    pub severity: String,
    pub title: String,
    pub detail: Value,
    pub model: String,
    pub now: i64,
}

const FINDING_COLUMNS: &str = "finding_id, chat_id, source, trigger_kind, severity, \
                               title, detail, model, created_at, escalated";

fn read_finding(row: &rusqlite::Row<'_>) -> Result<AdvisorFindingRecord> {
    let detail: String = row.get(6)?;
    Ok(AdvisorFindingRecord {
        finding_id: row.get(0)?,
        chat_id: row.get(1)?,
        source: row.get(2)?,
        trigger_kind: row.get(3)?,
        severity: row.get(4)?,
        title: row.get(5)?,
        detail: serde_json::from_str(&detail)?,
        model: row.get(7)?,
        created_at: row.get(8)?,
        escalated: row.get::<_, i64>(9)? != 0,
    })
}

fn select_finding(db: &Connection, finding_id: &str) -> Result<Option<AdvisorFindingRecord>> {
    let mut statement = db.prepare(&format!(
        "SELECT {FINDING_COLUMNS} FROM advisor_findings WHERE finding_id=?"
    ))?;
    let mut rows = statement.query_and_then([finding_id], read_finding)?;
    match rows.next() {
        Some(row) => Ok(Some(row?)),
        None => Ok(None),
    }
}

impl SqliteStore {
    /// Record one finding. A duplicate `finding_id` is a retry of the same
    /// report: the existing row is returned unchanged. An unknown source
    /// or severity, or a non-object `detail`, is an invalid input — the
    /// database CHECKs never surface as store errors on this path.
    pub async fn insert_advisor_finding(
        &self,
        input: InsertFinding,
    ) -> Result<AdvisorFindingRecord> {
        if !valid_source(&input.source) {
            return Err(StoreError::InvalidInput(format!(
                "invalid finding source: {}",
                input.source
            )));
        }
        if !valid_severity(&input.severity) {
            return Err(StoreError::InvalidInput(format!(
                "invalid finding severity: {}",
                input.severity
            )));
        }
        if !input.detail.is_object() {
            return Err(StoreError::InvalidInput(
                "finding detail must be a JSON object".into(),
            ));
        }
        // Model-authored text is capped at the durable boundary: the
        // shapes stay, lengths get bounded, truncation is marked.
        let mut input = input;
        input.title = crate::subagents::cap_text(&input.title, MAX_TITLE_CHARS);
        cap_detail(&mut input.detail, MAX_DETAIL_STRING_CHARS);
        self.call(move |db| {
            if let Some(existing) = select_finding(db, &input.finding_id)? {
                return Ok(existing);
            }
            db.execute(
                "INSERT INTO advisor_findings(finding_id,chat_id,source,trigger_kind,\
                 severity,title,detail,model,created_at,escalated) \
                 VALUES(?,?,?,?,?,?,?,?,?,0)",
                params![
                    input.finding_id,
                    input.chat_id,
                    input.source,
                    input.trigger_kind,
                    input.severity,
                    input.title,
                    serde_json::to_string(&input.detail)?,
                    input.model,
                    input.now,
                ],
            )?;
            select_finding(db, &input.finding_id)?
                .ok_or_else(|| StoreError::NotFound(format!("finding {}", input.finding_id)))
        })
        .await
    }

    /// The chat's findings, newest first (insertion order breaking equal
    /// timestamps), clamped to `1..=200` rows.
    pub async fn list_advisor_findings(
        &self,
        chat_id: &str,
        limit: i64,
    ) -> Result<Vec<AdvisorFindingRecord>> {
        let chat_id = chat_id.to_owned();
        let limit = limit.clamp(1, 200);
        self.call(move |db| {
            let mut statement = db.prepare(&format!(
                "SELECT {FINDING_COLUMNS} FROM advisor_findings WHERE chat_id=? \
                 ORDER BY created_at DESC, rowid DESC LIMIT ?"
            ))?;
            let records = statement
                .query_and_then(params![chat_id, limit], read_finding)?
                .collect::<Result<Vec<_>>>()?;
            Ok(records)
        })
        .await
    }

    /// Mark a finding as already escalated to the user. `false` when the
    /// row is missing; re-marking an escalated finding is an idempotent
    /// no-op returning `false` (nothing changed).
    pub async fn mark_advisor_finding_escalated(&self, finding_id: &str) -> Result<bool> {
        let finding_id = finding_id.to_owned();
        self.call(move |db| {
            let updated = db.execute(
                "UPDATE advisor_findings SET escalated=1 WHERE finding_id=? AND escalated=0",
                params![finding_id],
            )?;
            Ok(updated > 0)
        })
        .await
    }

    /// Findings created at or after `since`, optionally narrowed to one
    /// chat and one source: the watchdog rate-limit window's predicate.
    pub async fn count_advisor_findings_since(
        &self,
        chat_id: Option<&str>,
        source: Option<&str>,
        since: i64,
    ) -> Result<i64> {
        let chat_id = chat_id.map(str::to_owned);
        let source = source.map(str::to_owned);
        self.call(move |db| {
            let mut sql = String::from("SELECT count(*) FROM advisor_findings WHERE created_at>=?");
            let mut args: Vec<String> = Vec::new();
            if let Some(chat_id) = &chat_id {
                sql.push_str(" AND chat_id=?");
                args.push(chat_id.clone());
            }
            if let Some(source) = &source {
                sql.push_str(" AND source=?");
                args.push(source.clone());
            }
            let mut bindings: Vec<&dyn rusqlite::ToSql> = vec![&since];
            for arg in &args {
                bindings.push(arg);
            }
            let count = db.query_row(&sql, rusqlite::params_from_iter(bindings), |row| {
                row.get::<_, i64>(0)
            })?;
            Ok(count)
        })
        .await
    }

    /// Record a STARTED review attempt (the watchdog window's honest
    /// denominator: zero-finding reviews must count too). Re-recording the
    /// same `run_id` refreshes the row — an in-flight retry, not a new
    /// attempt.
    pub async fn record_advisor_review(
        &self,
        run_id: &str,
        chat_id: &str,
        origin: &str,
        trigger_kind: &str,
        started_at: i64,
    ) -> Result<()> {
        if !valid_source(origin) {
            return Err(StoreError::InvalidInput(format!(
                "invalid review origin: {origin}"
            )));
        }
        let run_id = run_id.to_owned();
        let chat_id = chat_id.to_owned();
        let origin = origin.to_owned();
        let trigger_kind = trigger_kind.to_owned();
        self.call(move |db| {
            db.execute(
                "INSERT INTO advisor_reviews(run_id,chat_id,origin,trigger_kind,started_at) \
                 VALUES(?,?,?,?,?) \
                 ON CONFLICT(run_id) DO UPDATE SET started_at=excluded.started_at",
                params![run_id, chat_id, origin, trigger_kind, started_at],
            )?;
            Ok(())
        })
        .await
    }

    /// Atomic check-then-record: inserts the attempt row only when fewer than
    /// `limit` reviews for this (chat, origin) started at/after `since`.
    /// Count and insert run in ONE actor job, so concurrent reservers can
    /// never overshoot `limit`. `Ok(false)` = over budget, nothing recorded.
    #[allow(clippy::too_many_arguments)]
    pub async fn reserve_advisor_review_if_under(
        &self,
        run_id: &str,
        chat_id: &str,
        origin: &str,
        trigger_kind: &str,
        started_at: i64,
        since: i64,
        limit: i64,
    ) -> Result<bool> {
        if !valid_source(origin) {
            return Err(StoreError::InvalidInput(format!(
                "invalid review origin: {origin}"
            )));
        }
        let run_id = run_id.to_owned();
        let chat_id = chat_id.to_owned();
        let origin = origin.to_owned();
        let trigger_kind = trigger_kind.to_owned();
        self.call(move |db| {
            // IMMEDIATE: take the write lock before counting, so another
            // process on the same file cannot slip between count and insert.
            let transaction = crate::transaction::atomic_with_behavior(
                db,
                rusqlite::TransactionBehavior::Immediate,
            )?;
            let count: i64 = transaction.query_row(
                "SELECT count(*) FROM advisor_reviews \
                 WHERE started_at>=? AND chat_id=? AND origin=?",
                params![since, chat_id, origin],
                |row| row.get(0),
            )?;
            if count >= limit {
                return Ok(false);
            }
            transaction.execute(
                "INSERT INTO advisor_reviews(run_id,chat_id,origin,trigger_kind,started_at) \
                 VALUES(?,?,?,?,?) \
                 ON CONFLICT(run_id) DO UPDATE SET started_at=excluded.started_at",
                params![run_id, chat_id, origin, trigger_kind, started_at],
            )?;
            transaction.commit()?;
            Ok(true)
        })
        .await
    }

    /// Reviews STARTED since `since`, optionally per chat / origin — the
    /// watchdog's durable rolling-window budget.
    pub async fn count_advisor_reviews_since(
        &self,
        chat_id: Option<&str>,
        origin: Option<&str>,
        since: i64,
    ) -> Result<i64> {
        let chat_id = chat_id.map(str::to_owned);
        let origin = origin.map(str::to_owned);
        self.call(move |db| {
            let mut sql = String::from("SELECT count(*) FROM advisor_reviews WHERE started_at>=?");
            let mut args: Vec<String> = Vec::new();
            if let Some(chat_id) = &chat_id {
                sql.push_str(" AND chat_id=?");
                args.push(chat_id.clone());
            }
            if let Some(origin) = &origin {
                sql.push_str(" AND origin=?");
                args.push(origin.clone());
            }
            let mut bindings: Vec<&dyn rusqlite::ToSql> = vec![&since];
            for arg in &args {
                bindings.push(arg);
            }
            let count = db.query_row(&sql, rusqlite::params_from_iter(bindings), |row| {
                row.get::<_, i64>(0)
            })?;
            Ok(count)
        })
        .await
    }
}
