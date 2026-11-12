//! Durable native-execution authority (AGENTKIT-112/115): the vendor
//! session lineage ledger and the pending-interaction ledger.
//!
//! Both ledgers commit in the same transaction family as the task rows
//! they describe. Rules the store enforces mechanically; the host's
//! executor adds lease/authority context on top:
//!
//! * one binding row per attempt, bound BEFORE spawn (`assigned`) and
//!   confirmed from the vendor's own init frame (`observed`); exact resume
//!   reads only the latest OBSERVED binding — there is no "continue
//!   latest" guess (AGENTKIT-107 §5);
//! * interactions are compare-and-set: the first settlement stands,
//!   identical `answer_id` redelivery is an idempotent `Duplicate`, and
//!   every refusal names a durable-visible reason (AGENTKIT-115).

use rusqlite::params;
use serde::{Deserialize, Serialize};

use agentkit_native::{
    AnswerDisposition, AnswerSubmission, BindNativeSession, Decision, InteractionKind,
    InteractionRejection, NativeSessionBinding, PendingInteraction,
};

use crate::{Result, StoreError};

/// A stored interaction with its settlement facts.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InteractionRow {
    pub record: PendingInteraction,
    /// `pending` | `answered` | `expired` | `invalidated`.
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<AnswerSubmission>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answered_at_ms: Option<i64>,
}

impl crate::SqliteStore {
    /// Record the frozen vendor session identity for an attempt before
    /// spawn. Idempotent for an identical redelivery; a different identity
    /// for the same attempt is a hard input error (frozen facts never
    /// mutate).
    pub async fn bind_native_session(
        &self,
        input: BindNativeSession,
    ) -> Result<NativeSessionBinding> {
        let binding = NativeSessionBinding {
            attempt_id: input.attempt_id,
            task_id: input.task_id,
            backend: input.backend,
            vendor_session_id: input.vendor_session_id,
            parent_vendor_session_id: input.parent_vendor_session_id,
            state: "assigned".into(),
            assigned_at: input.assigned_at,
            observed_at: None,
        };
        if binding.vendor_session_id.trim().is_empty() || binding.backend.trim().is_empty() {
            return Err(StoreError::InvalidInput(
                "native binding needs backend and vendor session id".into(),
            ));
        }
        let expected = binding.clone();
        self.call(move |db| {
            if let Some(existing) = read_binding(db, &expected.attempt_id)? {
                if existing.vendor_session_id != expected.vendor_session_id
                    || existing.backend != expected.backend
                    || existing.task_id != expected.task_id
                {
                    return Err(StoreError::InvalidInput(
                        "native binding is frozen: identity differs".into(),
                    ));
                }
                return Ok(existing);
            }
            db.execute(
                "INSERT INTO native_session_bindings VALUES(?,?,?,?,?,?,?,?)",
                params![
                    expected.attempt_id,
                    expected.task_id,
                    expected.backend,
                    expected.vendor_session_id,
                    expected.parent_vendor_session_id,
                    expected.state,
                    expected.assigned_at,
                    expected.observed_at,
                ],
            )?;
            Ok(expected)
        })
        .await
    }

    /// Confirm the vendor-reported identity from the init frame. The
    /// observed id must equal the assigned one — drift is refused loudly,
    /// never absorbed (AGENTKIT-113 lineage integrity).
    pub async fn observe_native_session(
        &self,
        attempt_id: &str,
        vendor_session_id: &str,
        now: i64,
    ) -> Result<NativeSessionBinding> {
        let attempt_id = attempt_id.to_owned();
        let vendor_session_id = vendor_session_id.to_owned();
        self.call(move |db| {
            let mut binding = read_binding(db, &attempt_id)?
                .ok_or_else(|| StoreError::NotFound(attempt_id.clone()))?;
            if binding.vendor_session_id != vendor_session_id {
                return Err(StoreError::InvalidInput(format!(
                    "vendor session drift: bound {}, observed {vendor_session_id}",
                    binding.vendor_session_id
                )));
            }
            if binding.state == "assigned" {
                db.execute(
                    "UPDATE native_session_bindings SET state='observed', observed_at=? WHERE attempt_id=?",
                    params![now, attempt_id],
                )?;
                binding.state = "observed".into();
                binding.observed_at = Some(now);
            }
            Ok(binding)
        })
        .await
    }

    /// The latest OBSERVED binding for a task — the only identity an exact
    /// resume may use. `None` means "no lineage": dispatch must choose
    /// Fresh (with seeded ancestry), never guess.
    pub async fn current_native_binding(
        &self,
        task_id: &str,
    ) -> Result<Option<NativeSessionBinding>> {
        let task_id = task_id.to_owned();
        self.call(move |db| {
            let mut statement = db.prepare(
                "SELECT attempt_id, task_id, backend, vendor_session_id, parent_vendor_session_id, state, assigned_at, observed_at FROM native_session_bindings WHERE task_id=?1 AND state='observed' ORDER BY assigned_at DESC LIMIT 1",
            )?;
            let mut rows = statement.query(params![task_id])?;
            match rows.next()? {
                Some(row) => Ok(Some(read_binding_row(row)?)),
                None => Ok(None),
            }
        })
        .await
    }

    /// Is this vendor identity OBSERVED under ANY task on this host?
    /// A new task exact-resuming the conversation's latest session has no
    /// binding rows of its own yet — the lineage lives on the previous
    /// task. `observed` is the only vendor-attested state (init frame), so
    /// a cross-task restatement is as trustworthy as the task-local one;
    /// `assigned` and `retired` identities never qualify (AGENTKIT-113).
    pub async fn is_observed_native_session(&self, vendor_session_id: &str) -> Result<bool> {
        let vendor_session_id = vendor_session_id.to_owned();
        self.call(move |db| {
            let mut statement = db.prepare(
                "SELECT 1 FROM native_session_bindings WHERE vendor_session_id=?1 AND state='observed' LIMIT 1",
            )?;
            Ok(statement.query(params![vendor_session_id])?.next()?.is_some())
        })
        .await
    }

    /// Mark an attempt's binding retired (task branch/retirement); the row
    /// stays for audit.
    pub async fn retire_native_binding(&self, attempt_id: &str) -> Result<()> {
        let attempt_id = attempt_id.to_owned();
        self.call(move |db| {
            let changed = db.execute(
                "UPDATE native_session_bindings SET state='retired' WHERE attempt_id=?1 AND state!='retired'",
                params![attempt_id],
            )?;
            if changed == 0 && read_binding(db, &attempt_id)?.is_none() {
                return Err(StoreError::NotFound(attempt_id));
            }
            Ok(())
        })
        .await
    }

    /// Persist a pending interaction. Idempotent for an identical
    /// redelivery (same id, same record); a conflicting re-issue under one
    /// id is an input error. The `record` column is written once and never
    /// rewritten: settlements update status/answer columns only, so audit
    /// always sees the exact request a human was shown.
    pub async fn open_interaction(&self, record: PendingInteraction) -> Result<()> {
        if record.expires_at_ms <= record.issued_at_ms {
            return Err(StoreError::InvalidInput(
                "interaction expiry must follow issue time".into(),
            ));
        }
        let expected = record.clone();
        self.call(move |db| {
            if let Some(prior) = read_interaction(db, &expected.interaction_id)? {
                if prior.record == expected {
                    return Ok(());
                }
                return Err(StoreError::InvalidInput(
                    "interaction id already carries a different record".into(),
                ));
            }
            db.execute(
                "INSERT INTO native_interactions VALUES(?,?,?,?,?,NULL,?,NULL)",
                params![
                    expected.interaction_id,
                    expected.task_id,
                    expected.attempt_id,
                    "pending",
                    serde_json::to_string(&expected)?,
                    expected.expires_at_ms,
                ],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn get_interaction(&self, interaction_id: &str) -> Result<Option<InteractionRow>> {
        let interaction_id = interaction_id.to_owned();
        self.call(move |db| read_interaction(db, &interaction_id))
            .await
    }

    /// A task's interactions, newest first; `only_pending` filters to
    /// `status='pending'`. Expired rows still carry their `expires_at_ms`
    /// — pollers should run [`SqliteStore::expire_interactions`] first (as
    /// the OneMind lane does) so the durable status tells the same story
    /// the UI shows; answering an expired row is always refused honestly.
    pub async fn list_interactions(
        &self,
        task_id: &str,
        only_pending: bool,
    ) -> Result<Vec<InteractionRow>> {
        let task_id = task_id.to_owned();
        self.call(move |db| {
            let sql = if only_pending {
                "SELECT record, status, answer, answered_at_ms FROM native_interactions WHERE task_id=?1 AND status='pending' ORDER BY rowid DESC"
            } else {
                "SELECT record, status, answer, answered_at_ms FROM native_interactions WHERE task_id=?1 ORDER BY rowid DESC"
            };
            let mut statement = db.prepare(sql)?;
            let rows = statement.query_map(params![task_id], read_interaction_row)?;
            rows.map(|row| row.map_err(StoreError::from)).collect()
        })
        .await
    }

    /// Compare-and-set answer. Only a `pending` row settles; the identity
    /// checks (task/attempt/generation/content hash) run against the
    /// stored record so answers cannot be replayed across attempts or
    /// against changed content. The first settlement stands; the same
    /// `answer_id` redelivered is an idempotent `Duplicate`.
    ///
    /// `require_live_authority` (AGENTKIT-115 hardening): when set, the
    /// settlement additionally happens in the SAME transaction against
    /// LIVE authority — the task must still be `running`/`waiting_approval`
    /// and its lease must still name the attempt. A stale UI answer to an
    /// interaction whose attempt is gone is refused `stale_attempt`
    /// instead of silently settling a dead row.
    pub async fn answer_interaction(
        &self,
        submission: &AnswerSubmission,
        expected_task_id: &str,
        expected_attempt_id: &str,
        expected_generation: u64,
        expected_content_hash: &str,
        now: i64,
    ) -> Result<AnswerDisposition> {
        self.answer_interaction_inner(
            submission,
            expected_task_id,
            expected_attempt_id,
            expected_generation,
            expected_content_hash,
            now,
            false,
        )
        .await
    }

    /// Fenced settlement: identical to [`Self::answer_interaction`] plus
    /// the live-authority transaction check (task live, lease names the
    /// attempt). Required for durable approval UIs (review §2.7).
    #[allow(clippy::too_many_arguments)]
    pub async fn answer_interaction_fenced(
        &self,
        submission: &AnswerSubmission,
        expected_task_id: &str,
        expected_attempt_id: &str,
        expected_generation: u64,
        expected_content_hash: &str,
        now: i64,
    ) -> Result<AnswerDisposition> {
        self.answer_interaction_inner(
            submission,
            expected_task_id,
            expected_attempt_id,
            expected_generation,
            expected_content_hash,
            now,
            true,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn answer_interaction_inner(
        &self,
        submission: &AnswerSubmission,
        expected_task_id: &str,
        expected_attempt_id: &str,
        expected_generation: u64,
        expected_content_hash: &str,
        now: i64,
        require_live_authority: bool,
    ) -> Result<AnswerDisposition> {
        let submission = submission.clone();
        let expected_task_id = expected_task_id.to_owned();
        let expected_attempt_id = expected_attempt_id.to_owned();
        let expected_content_hash = expected_content_hash.to_owned();
        self.call(move |db| {
            let transaction = crate::transaction::atomic(db)?;
            let Some(row) = read_interaction(&transaction, &submission.interaction_id)? else {
                return Ok(AnswerDisposition::Rejected {
                    reason: InteractionRejection::NotFound,
                });
            };
            let record = &row.record;
            // Identity binding (never trust the caller's framing).
            if record.task_id != expected_task_id {
                return Ok(AnswerDisposition::Rejected {
                    reason: InteractionRejection::CrossTask,
                });
            }
            if record.attempt_id != expected_attempt_id {
                return Ok(AnswerDisposition::Rejected {
                    reason: InteractionRejection::StaleAttempt,
                });
            }
            if record.process_generation != expected_generation {
                return Ok(AnswerDisposition::Rejected {
                    reason: InteractionRejection::StaleGeneration,
                });
            }
            if record.content_hash != expected_content_hash {
                return Ok(AnswerDisposition::Rejected {
                    reason: InteractionRejection::InputChanged,
                });
            }
            // The decision must fit the kind.
            let kind_ok = matches!(
                (record.kind, &submission.decision),
                (InteractionKind::Approval, Decision::Allow | Decision::Deny)
                    | (InteractionKind::Question, Decision::Respond { .. })
            );
            if !kind_ok {
                return Ok(AnswerDisposition::Rejected {
                    reason: InteractionRejection::InputChanged,
                });
            }
            // Settlement is monotone.
            if row.status != "pending" {
                return Ok(if row.answer.as_ref().is_some_and(|prior| {
                    prior.answer_id == submission.answer_id
                }) {
                    AnswerDisposition::Duplicate
                } else {
                    AnswerDisposition::Rejected {
                        reason: InteractionRejection::AlreadyAnswered,
                    }
                });
            }
            if now > record.expires_at_ms {
                transaction.execute(
                    "UPDATE native_interactions SET status='expired' WHERE interaction_id=?1 AND status='pending'",
                    params![submission.interaction_id],
                )?;
                transaction.commit()?;
                return Ok(AnswerDisposition::Rejected {
                    reason: InteractionRejection::Expired,
                });
            }
            if require_live_authority {
                // Live authority, in the settlement transaction: the SAME
                // current-owner rule every durable claim applies — task
                // executing, no cancellation request, lease present,
                // naming the attempt, and unexpired. A stale UI answer
                // cannot settle a dead row, an expired lease, or a
                // cancelled attempt (review §2.7).
                if crate::native_exec::live_authority_gap(
                    &transaction,
                    &record.task_id,
                    &record.attempt_id,
                    now,
                )?
                    .is_some()
                {
                    return Ok(AnswerDisposition::Rejected {
                        reason: InteractionRejection::StaleAttempt,
                    });
                }
            }
            let updated = transaction.execute(
                "UPDATE native_interactions SET status='answered', answer=?2, answered_at_ms=?3 WHERE interaction_id=?1 AND status='pending'",
                params![
                    submission.interaction_id,
                    serde_json::to_string(&submission)?,
                    now,
                ],
            )?;
            if updated == 0 {
                // Lost the race to a concurrent settlement under the same
                // lease: re-read and classify honestly.
                let current = read_interaction(&transaction, &submission.interaction_id)?;
                let duplicate = current
                    .and_then(|current| current.answer)
                    .is_some_and(|prior| prior.answer_id == submission.answer_id);
                transaction.commit()?;
                return Ok(if duplicate {
                    AnswerDisposition::Duplicate
                } else {
                    AnswerDisposition::Rejected {
                        reason: InteractionRejection::AlreadyAnswered,
                    }
                });
            }
            transaction.commit()?;
            Ok(AnswerDisposition::Accepted {
                answered_at_ms: now,
            })
        })
        .await
    }

    /// Expire pending interactions whose clock ran out (called on poll and
    /// recovery). Returns the number expired.
    pub async fn expire_interactions(&self, task_id: &str, now: i64) -> Result<usize> {
        let task_id = task_id.to_owned();
        self.call(move |db| {
            Ok(db.execute(
                "UPDATE native_interactions SET status='expired' WHERE task_id=?1 AND status='pending' AND expires_at_ms<=?2",
                params![task_id, now],
            )?)
        })
        .await
    }

    /// Invalidate one still-pending interaction by id (review R5: the
    /// request whose wait produced this approval gate was ABANDONED — its
    /// half of the conversation is gone, so the gate must not linger
    /// answerable). A row already settled by a human, expired, or
    /// invalidated is left untouched; the return says whether this call
    /// performed the invalidation.
    pub async fn invalidate_interaction(&self, interaction_id: &str) -> Result<bool> {
        let interaction_id = interaction_id.to_owned();
        self.call(move |db| {
            Ok(db.execute(
                "UPDATE native_interactions SET status='invalidated' WHERE interaction_id=?1 AND status='pending'",
                params![interaction_id],
            )?
                > 0)
        })
        .await
    }

    /// Invalidate every still-pending interaction of one attempt (cancel,
    /// interruption, process replacement). Returns the number invalidated.
    pub async fn invalidate_interactions(&self, attempt_id: &str, now: i64) -> Result<usize> {
        let attempt_id = attempt_id.to_owned();
        self.call(move |db| {
            let _ = now;
            Ok(db.execute(
                "UPDATE native_interactions SET status='invalidated' WHERE attempt_id=?1 AND status='pending'",
                params![attempt_id],
            )?)
        })
        .await
    }
}

/// The CURRENT-OWNER rule (AGENTKIT-115) as one reusable transaction
/// probe: the attempt must be the one named by the task's live lease,
/// the lease must be unexpired, the task must sit in an executing
/// state, and no cancellation may have been requested. `Ok(None)` is
/// authorized; `Ok(Some(reason))` names the missing condition. Used by
/// the fenced interaction settlement and by durable continuation
/// commits alike, so the authority rules cannot drift apart.
pub(crate) fn live_authority_gap(
    db: &rusqlite::Connection,
    task_id: &str,
    attempt_id: &str,
    now: i64,
) -> Result<Option<&'static str>> {
    let task: Option<(String, bool)> = db
        .query_row(
            "SELECT status, record FROM tasks WHERE id=?1",
            params![task_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .ok()
        .map(|(status, record_raw)| {
            let cancel = serde_json::from_str::<serde_json::Value>(&record_raw)
                .ok()
                .and_then(|v| {
                    v.get("cancelRequested")
                        .and_then(serde_json::Value::as_bool)
                })
                .unwrap_or(false);
            (status, cancel)
        });
    let Some((status, cancel_requested)) = task else {
        return Ok(Some("task-gone"));
    };
    if !matches!(status.as_str(), "running" | "waiting_approval") {
        return Ok(Some("task-state"));
    }
    if cancel_requested {
        return Ok(Some("cancel-requested"));
    }
    let lease = db
        .query_row(
            "SELECT record FROM leases WHERE task_id=?1",
            params![task_id],
            |row| row.get::<_, String>(0),
        )
        .ok()
        .and_then(|raw| serde_json::from_str::<crate::Lease>(&raw).ok());
    match lease {
        None => Ok(Some("lease-gone")),
        Some(lease) if lease.attempt_id != attempt_id => Ok(Some("lease-attempt")),
        Some(lease) if lease.expires_at <= now => Ok(Some("lease-expired")),
        Some(_) => Ok(None),
    }
}

fn read_binding(
    db: &rusqlite::Connection,
    attempt_id: &str,
) -> Result<Option<NativeSessionBinding>> {
    let mut statement = db.prepare(
        "SELECT attempt_id, task_id, backend, vendor_session_id, parent_vendor_session_id, state, assigned_at, observed_at FROM native_session_bindings WHERE attempt_id=?1",
    )?;
    let mut rows = statement.query(params![attempt_id])?;
    match rows.next()? {
        Some(row) => Ok(Some(read_binding_row(row)?)),
        None => Ok(None),
    }
}

fn read_binding_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<NativeSessionBinding> {
    Ok(NativeSessionBinding {
        attempt_id: row.get(0)?,
        task_id: row.get(1)?,
        backend: row.get(2)?,
        vendor_session_id: row.get(3)?,
        parent_vendor_session_id: row.get(4)?,
        state: row.get(5)?,
        assigned_at: row.get(6)?,
        observed_at: row.get(7)?,
    })
}

fn read_interaction(
    db: &rusqlite::Connection,
    interaction_id: &str,
) -> Result<Option<InteractionRow>> {
    let mut statement = db.prepare(
        "SELECT record, status, answer, answered_at_ms FROM native_interactions WHERE interaction_id=?1",
    )?;
    let mut rows = statement.query(params![interaction_id])?;
    match rows.next()? {
        Some(row) => Ok(Some(read_interaction_row(row)?)),
        None => Ok(None),
    }
}

fn read_interaction_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<InteractionRow> {
    let record: String = row.get(0)?;
    let status: String = row.get(1)?;
    let answer: Option<String> = row.get(2)?;
    let answered_at_ms: Option<i64> = row.get(3)?;
    // Column readers cannot carry StoreError; surface decode failures as
    // rusqlite::Error::InvalidColumnIndex so the store maps them to a
    // recovery-unreadable class rather than silently dropping the row.
    serde_json::from_str(&record)
        .map_err(|e| rusqlite::Error::InvalidParameterName(e.to_string()))
        .and_then(|record| {
            answer
                .map(|answer| {
                    serde_json::from_str(&answer)
                        .map_err(|e| rusqlite::Error::InvalidParameterName(e.to_string()))
                })
                .transpose()
                .map(|answer| InteractionRow {
                    record,
                    status,
                    answer,
                    answered_at_ms,
                })
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn record(task: &str, attempt: &str) -> PendingInteraction {
        PendingInteraction {
            interaction_id: format!("int-{task}-{attempt}"),
            task_id: task.into(),
            attempt_id: attempt.into(),
            process_generation: 3,
            invocation_id: "inv-1".into(),
            kind: InteractionKind::Approval,
            content: json!({"tool":"search","argsHash":"a"}),
            content_hash: "hash-a".into(),
            actor_id: "user-1".into(),
            issued_at_ms: 1_000,
            expires_at_ms: 10_000,
        }
    }

    fn store() -> crate::SqliteStore {
        crate::SqliteStore::memory(64).unwrap()
    }

    #[tokio::test]
    async fn binding_freezes_identity_and_confirms_once() {
        let store = store();
        submit_task(&store, "task-1").await;
        let bind = BindNativeSession {
            attempt_id: "att-1".into(),
            task_id: "task-1".into(),
            backend: "claude-code".into(),
            vendor_session_id: "sess-9".into(),
            parent_vendor_session_id: None,
            assigned_at: 5,
        };
        let first = store.bind_native_session(bind.clone()).await.unwrap();
        assert_eq!(first.state, "assigned");
        // Identical redelivery is idempotent.
        store.bind_native_session(bind.clone()).await.unwrap();
        // Identity change on the same attempt is refused.
        let mut drift = bind.clone();
        drift.vendor_session_id = "sess-other".into();
        assert!(store.bind_native_session(drift).await.is_err());
        // No lineage until observed.
        assert!(
            store
                .current_native_binding("task-1")
                .await
                .unwrap()
                .is_none()
        );
        // Observation must match the bound identity.
        assert!(
            store
                .observe_native_session("att-1", "sess-wrong", 6)
                .await
                .is_err()
        );
        let observed = store
            .observe_native_session("att-1", "sess-9", 6)
            .await
            .unwrap();
        assert_eq!(observed.state, "observed");
        assert_eq!(observed.observed_at, Some(6));
        let current = store
            .current_native_binding("task-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(current.attempt_id, "att-1");
        // Retirement keeps the row for audit but removes resume authority.
        store.retire_native_binding("att-1").await.unwrap();
        assert!(
            store
                .current_native_binding("task-1")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn is_observed_only_after_vendor_echo() {
        // Cross-task resume authority (AGENTKIT-341): an id is resumable
        // only once the vendor echoed it - assignment alone never is.
        let store = store();
        submit_task(&store, "task-a").await;
        let bind = BindNativeSession {
            attempt_id: "att-a".into(),
            task_id: "task-a".into(),
            backend: "claude-code".into(),
            vendor_session_id: "sess-x".into(),
            parent_vendor_session_id: None,
            assigned_at: 5,
        };
        store.bind_native_session(bind).await.unwrap();
        assert!(
            !store.is_observed_native_session("sess-x").await.unwrap(),
            "assigned-but-never-echoed ids are phantoms"
        );
        store
            .observe_native_session("att-a", "sess-x", 6)
            .await
            .unwrap();
        assert!(store.is_observed_native_session("sess-x").await.unwrap());
        // Retirement removes resume authority, matching the binding read.
        store.retire_native_binding("att-a").await.unwrap();
        assert!(!store.is_observed_native_session("sess-x").await.unwrap());
        assert!(
            !store
                .is_observed_native_session("sess-never")
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn binding_requires_task_and_identity() {
        let store = store();
        // Unknown task id fails the foreign key, not silently.
        let err = store
            .bind_native_session(BindNativeSession {
                attempt_id: "att-x".into(),
                task_id: "missing-task".into(),
                backend: "claude-code".into(),
                vendor_session_id: "s".into(),
                parent_vendor_session_id: None,
                assigned_at: 1,
            })
            .await;
        assert!(err.is_err());
        // Empty identity is refused up front.
        assert!(
            store
                .bind_native_session(BindNativeSession {
                    attempt_id: "att-y".into(),
                    task_id: "task-1".into(),
                    backend: "".into(),
                    vendor_session_id: "s".into(),
                    parent_vendor_session_id: None,
                    assigned_at: 1,
                })
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn answer_is_cas_with_full_identity_binding() {
        let store = store();
        // A task row must exist for the FK; use the submit path minimally.
        submit_task(&store, "task-1").await;
        store
            .open_interaction(record("task-1", "att-1"))
            .await
            .unwrap();
        // Idempotent redelivery.
        store
            .open_interaction(record("task-1", "att-1"))
            .await
            .unwrap();

        let answer = AnswerSubmission {
            interaction_id: "int-task-1-att-1".into(),
            answer_id: "ans-1".into(),
            decision: Decision::Allow,
            note: None,
        };
        let ok = store
            .answer_interaction(&answer, "task-1", "att-1", 3, "hash-a", 5_000)
            .await
            .unwrap();
        assert!(matches!(
            ok,
            AnswerDisposition::Accepted {
                answered_at_ms: 5_000
            }
        ));
        // Same answer id again is Duplicate, not a second effect.
        let dup = store
            .answer_interaction(&answer, "task-1", "att-1", 3, "hash-a", 5_100)
            .await
            .unwrap();
        assert_eq!(dup, AnswerDisposition::Duplicate);
        // A different answer now cannot settle it.
        let other = AnswerSubmission {
            answer_id: "ans-2".into(),
            ..answer.clone()
        };
        let rejected = store
            .answer_interaction(&other, "task-1", "att-1", 3, "hash-a", 5_200)
            .await
            .unwrap();
        assert_eq!(
            rejected,
            AnswerDisposition::Rejected {
                reason: InteractionRejection::AlreadyAnswered
            }
        );

        // Identity mismatches each map to their own rejection.
        store
            .open_interaction(record("task-1", "att-2"))
            .await
            .unwrap();
        let answer2 = AnswerSubmission {
            interaction_id: "int-task-1-att-2".into(),
            answer_id: "ans-m".into(),
            decision: Decision::Allow,
            note: None,
        };
        for (task, attempt, generation, hash, reason) in [
            (
                "task-2",
                "att-2",
                3,
                "hash-a",
                InteractionRejection::CrossTask,
            ),
            (
                "task-1",
                "att-9",
                3,
                "hash-a",
                InteractionRejection::StaleAttempt,
            ),
            (
                "task-1",
                "att-2",
                4,
                "hash-a",
                InteractionRejection::StaleGeneration,
            ),
            (
                "task-1",
                "att-2",
                3,
                "hash-b",
                InteractionRejection::InputChanged,
            ),
        ] {
            let mismatch = store
                .answer_interaction(&answer2, task, attempt, generation, hash, 5_000)
                .await
                .unwrap();
            assert_eq!(
                mismatch,
                AnswerDisposition::Rejected { reason },
                "{task}/{attempt}/{generation}/{hash}"
            );
            // The interaction remains pending after refusals.
            let row = store
                .get_interaction("int-task-1-att-2")
                .await
                .unwrap()
                .unwrap();
            assert_eq!(row.status, "pending");
        }

        // Unknown id.
        let missing = store
            .answer_interaction(
                &AnswerSubmission {
                    interaction_id: "int-nope".into(),
                    answer_id: "a".into(),
                    decision: Decision::Allow,
                    note: None,
                },
                "task-1",
                "att-1",
                3,
                "hash-a",
                5_000,
            )
            .await
            .unwrap();
        assert_eq!(
            missing,
            AnswerDisposition::Rejected {
                reason: InteractionRejection::NotFound
            }
        );
    }

    #[tokio::test]
    async fn expiry_and_invalidation_are_durable() {
        let store = store();
        submit_task(&store, "task-1").await;
        store
            .open_interaction(record("task-1", "att-1"))
            .await
            .unwrap();
        // Answer after expiry is refused AND the row moves to expired.
        let late = store
            .answer_interaction(
                &AnswerSubmission {
                    interaction_id: "int-task-1-att-1".into(),
                    answer_id: "a".into(),
                    decision: Decision::Allow,
                    note: None,
                },
                "task-1",
                "att-1",
                3,
                "hash-a",
                10_001,
            )
            .await
            .unwrap();
        assert_eq!(
            late,
            AnswerDisposition::Rejected {
                reason: InteractionRejection::Expired
            }
        );
        let row = store
            .get_interaction("int-task-1-att-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, "expired");

        // Bulk expire for a fresh pending set.
        store
            .open_interaction(record("task-1", "att-2"))
            .await
            .unwrap();
        let expired = store.expire_interactions("task-1", 10_001).await.unwrap();
        assert_eq!(expired, 1);
        // Bulk invalidate for a live set.
        store
            .open_interaction(record("task-1", "att-3"))
            .await
            .unwrap();
        let invalidated = store.invalidate_interactions("att-3", 6_000).await.unwrap();
        assert_eq!(invalidated, 1);
        let row = store
            .get_interaction("int-task-1-att-3")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, "invalidated");
        // Pending-only listing skips settled rows.
        let pending = store.list_interactions("task-1", true).await.unwrap();
        assert!(pending.iter().all(|r| r.status == "pending"));
    }

    #[tokio::test]
    async fn single_interaction_invalidation_is_surgical() {
        // Review R5: an abandoned approval wait invalidates ITS OWN row
        // only — a concurrently pending sibling (another live call of the
        // same attempt) must stay answerable, and a row a human already
        // settled must never be rewritten.
        let store = store();
        submit_task(&store, "task-1").await;
        store
            .open_interaction(record("task-1", "att-single-a"))
            .await
            .unwrap();
        let mut sibling = record("task-1", "att-single-b");
        sibling.interaction_id = "int-sibling".to_owned();
        store.open_interaction(sibling).await.unwrap();
        // A settled row of the same task.
        let mut settled = record("task-1", "att-single-c");
        settled.interaction_id = "int-settled".to_owned();
        store.open_interaction(settled).await.unwrap();
        // Human-settled row, marked final directly (the fenced answer
        // path has its own coverage; here we only need a non-pending
        // status to prove the invalidation filter).
        store
            .call(move |db| {
                db.execute(
                    "UPDATE native_interactions SET status='answered' WHERE interaction_id='int-settled'",
                    [],
                )?;
                Ok(())
            })
            .await
            .unwrap();

        assert!(
            store
                .invalidate_interaction("int-task-1-att-single-a")
                .await
                .unwrap()
        );
        let invalidated = store
            .get_interaction("int-task-1-att-single-a")
            .await
            .unwrap();
        assert_eq!(invalidated.unwrap().status, "invalidated");
        // The sibling wait is untouched — one abandoned call must not
        // strand its neighbours.
        assert_eq!(
            store
                .get_interaction("int-sibling")
                .await
                .unwrap()
                .unwrap()
                .status,
            "pending"
        );
        // A settled row is never rewritten, and re-invalidating an
        // invalidated row reports no work done.
        assert_eq!(
            store
                .get_interaction("int-settled")
                .await
                .unwrap()
                .unwrap()
                .status,
            "answered"
        );
        assert!(
            !store
                .invalidate_interaction("int-task-1-att-single-a")
                .await
                .unwrap()
        );
        assert!(!store.invalidate_interaction("int-nope").await.unwrap());
    }

    #[tokio::test]
    async fn decision_must_fit_kind() {
        let store = store();
        submit_task(&store, "task-1").await;
        let mut record = record("task-1", "att-1");
        record.interaction_id = "int-q".into();
        record.kind = InteractionKind::Question;
        record.content = json!({"question":"which?"});
        store.open_interaction(record).await.unwrap();
        // Allow is not an answer to a question.
        let wrong = store
            .answer_interaction(
                &AnswerSubmission {
                    interaction_id: "int-q".into(),
                    answer_id: "a".into(),
                    decision: Decision::Allow,
                    note: None,
                },
                "task-1",
                "att-1",
                3,
                "hash-a",
                5_000,
            )
            .await
            .unwrap();
        assert_eq!(
            wrong,
            AnswerDisposition::Rejected {
                reason: InteractionRejection::InputChanged
            }
        );
        // A response settles it.
        let right = store
            .answer_interaction(
                &AnswerSubmission {
                    interaction_id: "int-q".into(),
                    answer_id: "b".into(),
                    decision: Decision::Respond {
                        value: json!("first"),
                    },
                    note: None,
                },
                "task-1",
                "att-1",
                3,
                "hash-a",
                5_000,
            )
            .await
            .unwrap();
        assert!(matches!(right, AnswerDisposition::Accepted { .. }));
    }

    #[tokio::test]
    async fn conflicting_reissue_under_one_id_is_refused() {
        let store = store();
        submit_task(&store, "task-1").await;
        store
            .open_interaction(record("task-1", "att-1"))
            .await
            .unwrap();
        let mut changed = record("task-1", "att-1");
        changed.content = json!({"tool":"different"});
        assert!(store.open_interaction(changed).await.is_err());
        // Inverted clock is refused.
        let mut broken = record("task-1", "att-9");
        broken.interaction_id = "int-broken".into();
        broken.expires_at_ms = broken.issued_at_ms;
        assert!(store.open_interaction(broken).await.is_err());
    }

    #[tokio::test]
    async fn fenced_answer_requires_live_task_and_lease_for_the_attempt() {
        let store = store();
        submit_task(&store, "task-1").await;
        let claimed = store
            .claim(crate::ClaimInput {
                owner_id: "w-1".into(),
                now: 4_000,
                ttl_ms: 600_000,
                kinds: vec!["native.turn".into()],
                busy_scopes: vec![],
                max_attempts: 5,
            })
            .await
            .unwrap()
            .expect("claimable");
        let live_attempt = claimed.lease.attempt_id.clone();
        // An interaction owned by an attempt the lease does NOT name:
        // fenced settlement refuses it as a stale attempt.
        let mut stale = record("task-1", "att-dead");
        stale.interaction_id = "int-task-1-att-dead".into();
        store.open_interaction(stale).await.unwrap();
        let stale_submission = AnswerSubmission {
            interaction_id: "int-task-1-att-dead".into(),
            answer_id: "a-0".into(),
            decision: Decision::Allow,
            note: None,
        };
        let refused = store
            .answer_interaction_fenced(&stale_submission, "task-1", "att-dead", 3, "hash-a", 5_000)
            .await
            .unwrap();
        assert_eq!(
            refused,
            AnswerDisposition::Rejected {
                reason: InteractionRejection::StaleAttempt
            },
            "lease names a different attempt: no live authority"
        );
        assert_eq!(
            store
                .get_interaction("int-task-1-att-dead")
                .await
                .unwrap()
                .unwrap()
                .status,
            "pending",
            "refusal must not settle"
        );
        // The plain compatibility path (no live-authority requirement)
        // still settles, preserving pre-hardening consumer behavior.
        let loose = store
            .answer_interaction(&stale_submission, "task-1", "att-dead", 3, "hash-a", 5_000)
            .await
            .unwrap();
        assert!(
            matches!(loose, AnswerDisposition::Accepted { .. }),
            "compat path unchanged: {loose:?}"
        );
        // The interaction the live lease names settles atomically.
        let live = record("task-1", &live_attempt);
        let live_id = live.interaction_id.clone();
        store.open_interaction(live).await.unwrap();
        let submission = AnswerSubmission {
            interaction_id: live_id,
            answer_id: "a-1".into(),
            decision: Decision::Allow,
            note: None,
        };
        let accepted = store
            .answer_interaction_fenced(&submission, "task-1", &live_attempt, 3, "hash-a", 5_000)
            .await
            .unwrap();
        assert!(
            matches!(accepted, AnswerDisposition::Accepted { .. }),
            "live lease-backed attempt must settle: {accepted:?}"
        );
    }

    /// Review round-3 (coverage): the fenced answer path's ADDED authority
    /// conditions — cancellation request and lease expiry — must refuse
    /// through the ANSWER API itself, not only via the shared probe as
    /// exercised by `commit_continuation`.
    #[tokio::test]
    async fn fenced_answer_refuses_cancel_requested_and_expired_lease() {
        // (a) cancel-requested: the task still reads RUNNING, but the
        // cancellation request is durable — a fenced answer is stale.
        let store_a = store();
        submit_task(&store_a, "task-1").await;
        let claimed = store_a
            .claim(crate::ClaimInput {
                owner_id: "w-1".into(),
                now: 4_000,
                ttl_ms: 600_000,
                kinds: vec!["native.turn".into()],
                busy_scopes: vec![],
                max_attempts: 5,
            })
            .await
            .unwrap()
            .expect("claimable");
        let attempt = claimed.lease.attempt_id.clone();
        let live = record("task-1", &attempt);
        let live_id = live.interaction_id.clone();
        store_a.open_interaction(live).await.unwrap();
        store_a.cancel_task("task-1", 4_500).await.unwrap();
        let submission = AnswerSubmission {
            interaction_id: live_id.clone(),
            answer_id: "a-cancel".into(),
            decision: Decision::Allow,
            note: None,
        };
        let denied = store_a
            .answer_interaction_fenced(&submission, "task-1", &attempt, 3, "hash-a", 5_000)
            .await
            .unwrap();
        assert_eq!(
            denied,
            AnswerDisposition::Rejected {
                reason: InteractionRejection::StaleAttempt
            },
            "a cancellation request ends answer authority: {denied:?}"
        );
        assert_eq!(
            store_a
                .get_interaction(&live_id)
                .await
                .unwrap()
                .unwrap()
                .status,
            "pending",
            "refusal must not settle"
        );

        // (b) lease-expired: unexpired interaction, expired LEASE — the
        // row must not settle from a dead lease.
        let store_b = store();
        submit_task(&store_b, "task-2").await;
        let claimed = store_b
            .claim(crate::ClaimInput {
                owner_id: "w-2".into(),
                now: 4_000,
                ttl_ms: 1_000,
                kinds: vec!["native.turn".into()],
                busy_scopes: vec![],
                max_attempts: 5,
            })
            .await
            .unwrap()
            .expect("claimable");
        let attempt = claimed.lease.attempt_id.clone();
        let live = record("task-2", &attempt);
        let live_id = live.interaction_id.clone();
        store_b.open_interaction(live).await.unwrap();
        let submission = AnswerSubmission {
            interaction_id: live_id.clone(),
            answer_id: "a-expired".into(),
            decision: Decision::Allow,
            note: None,
        };
        let denied = store_b
            .answer_interaction_fenced(&submission, "task-2", &attempt, 3, "hash-a", 6_000)
            .await
            .unwrap();
        assert_eq!(
            denied,
            AnswerDisposition::Rejected {
                reason: InteractionRejection::StaleAttempt
            },
            "an expired lease ends answer authority: {denied:?}"
        );
        assert_eq!(
            store_b
                .get_interaction(&live_id)
                .await
                .unwrap()
                .unwrap()
                .status,
            "pending",
            "refusal must not settle"
        );
    }

    async fn submit_task(store: &crate::SqliteStore, id: &str) {
        use crate::SubmitTask;
        store
            .submit(SubmitTask {
                task_id: id.into(),
                kind: "native.turn".into(),
                scope_id: format!("scope-{id}"),
                payload: json!({}),
                priority: 0,
                now: 0,
                available_at: None,
                depends_on: vec![],
                exclusive_scope: false,
                parent_task_id: None,
            })
            .await
            .unwrap();
    }
}
