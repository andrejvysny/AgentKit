//! Scoped continuation bindings and durable broker call receipts
//! (review §2.3 / §2.7 hardening).
//!
//! A vendor session id alone is MUTABLE state, not an immutable transcript
//! snapshot: resuming it by global identity can silently attach a turn to a
//! different conversation, a different selected parent, or a different
//! configuration. A continuation binding therefore pins the FULL scope the
//! vendor session was proven against — conversation, selected parent,
//! acknowledged history position, backend, executable, config identity and
//! system-prompt revision — together with the owning attempt. Exact resume
//! must match every component; a binding is committed only by its owning
//! attempt after a successful completion, and supersession requires the
//! prior owner to have settled (fenced transitions).
//!
//! `native_call_receipts` gives governed broker calls a per-INVOCATION
//! durable receipt (identity is the invocation, never the argument hash):
//! an identical argument payload re-delivered under a different invocation
//! is a NEW call; the same invocation is an idempotent replay.

use rusqlite::params;
use serde::{Deserialize, Serialize};

use agentkit_native::canonical_digest;

use crate::{Result, StoreError};

/// One row of the scoped continuation ledger.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeContinuationBinding {
    pub binding_id: String,
    /// Consumer-defined conversation identity (opaque to the host).
    pub conversation: String,
    /// Consumer-defined selected-parent reference (e.g. parent message id).
    pub parent: String,
    /// Acknowledged history position at commit (monotonic per conversation).
    pub history_position: i64,
    pub backend: String,
    pub executable: String,
    pub config_identity: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt_revision: Option<String>,
    pub vendor_session_id: String,
    pub owning_attempt_id: String,
    pub owning_task_id: String,
    /// `active` | `superseded` | `retired`.
    pub state: String,
    pub committed_at: i64,
}

/// Commit input for one continuation binding (frozen facts from the
/// completed attempt's target + payload scope).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitContinuation {
    pub conversation: String,
    pub parent: String,
    pub history_position: i64,
    pub backend: String,
    pub executable: String,
    pub config_identity: String,
    pub system_prompt_revision: Option<String>,
    pub vendor_session_id: String,
    pub owning_attempt_id: String,
    pub owning_task_id: String,
    pub committed_at: i64,
}

/// One durable broker call receipt, keyed by attempt + invocation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeCallReceipt {
    pub attempt_id: String,
    pub task_id: String,
    pub invocation_id: String,
    pub tool_name: String,
    /// Integrity digest of the exact arguments — integrity, not identity.
    pub content_hash: String,
    /// Terminal outcome JSON recorded by the authority (executed/denied).
    pub record: serde_json::Value,
    pub recorded_at: i64,
}

/// Deterministic binding identity: same scope + session is the same row.
fn binding_id(input: &CommitContinuation) -> String {
    let digest = canonical_digest(&serde_json::json!({
        "conversation": input.conversation,
        "parent": input.parent,
        "historyPosition": input.history_position,
        "backend": input.backend,
        "executable": input.executable,
        "configIdentity": input.config_identity,
        "systemPromptRevision": input.system_prompt_revision,
        "vendorSessionId": input.vendor_session_id,
    }));
    format!("ncont_{}", digest.chars().take(32).collect::<String>())
}

impl crate::SqliteStore {
    /// Commit (or idempotently re-commit) the continuation binding an
    /// attempt earned by COMPLETING its turn. Supersession of a prior
    /// active binding on the same scope key is fenced: it only proceeds
    /// once the prior owning task has settled; a live prior owner is a
    /// hard refusal (never a silent handover).
    pub async fn commit_continuation(
        &self,
        input: CommitContinuation,
    ) -> Result<NativeContinuationBinding> {
        if input.conversation.trim().is_empty()
            || input.parent.trim().is_empty()
            || input.backend.trim().is_empty()
            || input.executable.trim().is_empty()
            || input.config_identity.trim().is_empty()
            || input.vendor_session_id.trim().is_empty()
            || input.owning_attempt_id.trim().is_empty()
            || input.owning_task_id.trim().is_empty()
            || input.history_position < 0
        {
            return Err(StoreError::InvalidInput(
                "continuation binding needs full scope, session and owner".into(),
            ));
        }
        let binding = NativeContinuationBinding {
            binding_id: binding_id(&input),
            conversation: input.conversation,
            parent: input.parent,
            history_position: input.history_position,
            backend: input.backend,
            executable: input.executable,
            config_identity: input.config_identity,
            system_prompt_revision: input.system_prompt_revision,
            vendor_session_id: input.vendor_session_id,
            owning_attempt_id: input.owning_attempt_id,
            owning_task_id: input.owning_task_id,
            state: "active".into(),
            committed_at: input.committed_at,
        };
        let expected = binding.clone();
        self.call(move |db| {
            let transaction = crate::transaction::atomic(db)?;
            if let Some(existing) = read_continuation(&transaction, &expected.binding_id)? {
                // Idempotent re-commit by the owning attempt only.
                if existing == expected {
                    return Ok(existing);
                }
                if existing.owning_attempt_id == expected.owning_attempt_id {
                    return Err(StoreError::InvalidInput(
                        "continuation binding is frozen: facts differ".into(),
                    ));
                }
                // A different attempt restating the same binding id is a
                // replay of that owner's commit only when it is itself
                // recorded there; otherwise it is a different commit.
                return Err(StoreError::Duplicate(
                    "continuation binding already committed by another attempt".into(),
                ));
            }
            // Supersede prior active bindings on the same scope key —
            // fenced on the prior owner having settled.
            let scope_key = (
                expected.conversation.clone(),
                expected.parent.clone(),
                expected.history_position,
                expected.backend.clone(),
                expected.executable.clone(),
                expected.config_identity.clone(),
            );
            let prior = find_active(&transaction, &scope_key)?;
            for prior in prior
                .into_iter()
                .filter(|prior| prior.binding_id != expected.binding_id)
            {
                let status: Option<String> = transaction
                    .query_row(
                        "SELECT status FROM tasks WHERE id=?1",
                        params![prior.owning_task_id],
                        |row| row.get(0),
                    )
                    .ok();
                let settled = match status.as_deref() {
                    None => true, // owner row gone: nothing can restate through it
                    Some(status) => matches!(status, "completed" | "failed" | "cancelled"),
                };
                if !settled {
                    return Err(StoreError::Duplicate(format!(
                        "continuation owner {} still holds the scope",
                        prior.owning_attempt_id
                    )));
                }
                transaction.execute(
                    "UPDATE native_continuations SET state='superseded' WHERE binding_id=?1 AND state='active'",
                    params![prior.binding_id],
                )?;
            }
            transaction.execute(
                "INSERT INTO native_continuations VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?)",
                params![
                    expected.binding_id,
                    expected.conversation,
                    expected.parent,
                    expected.history_position,
                    expected.backend,
                    expected.executable,
                    expected.config_identity,
                    expected.system_prompt_revision,
                    expected.vendor_session_id,
                    expected.owning_attempt_id,
                    expected.owning_task_id,
                    expected.state,
                    expected.committed_at,
                ],
            )?;
            transaction.commit()?;
            Ok(expected)
        })
        .await
    }

    /// The active binding for one exact scope, if any. Every component
    /// must match: this is what an exact resume must prove, and a miss is
    /// a miss (the caller chooses Fresh with seeded ancestry).
    #[allow(clippy::too_many_arguments)]
    pub async fn active_continuation(
        &self,
        conversation: &str,
        parent: &str,
        history_position: i64,
        backend: &str,
        executable: &str,
        config_identity: &str,
    ) -> Result<Option<NativeContinuationBinding>> {
        let (conversation, parent, backend, executable, config_identity) = (
            conversation.to_owned(),
            parent.to_owned(),
            backend.to_owned(),
            executable.to_owned(),
            config_identity.to_owned(),
        );
        self.call(move |db| {
            find_active(
                db,
                &(
                    conversation,
                    parent,
                    history_position,
                    backend,
                    executable,
                    config_identity,
                ),
            )
            .map(|rows| rows.into_iter().next())
        })
        .await
    }

    /// The binding committed by one attempt (audit, recovery).
    pub async fn continuation_for_attempt(
        &self,
        attempt_id: &str,
    ) -> Result<Option<NativeContinuationBinding>> {
        let attempt_id = attempt_id.to_owned();
        self.call(move |db| {
            let mut statement = db.prepare(
                "SELECT binding_id, conversation, parent, history_position, backend, executable, config_identity, system_prompt_revision, vendor_session_id, owning_attempt_id, owning_task_id, state, committed_at FROM native_continuations WHERE owning_attempt_id=?1 ORDER BY committed_at DESC LIMIT 1",
            )?;
            let mut rows = statement.query(params![attempt_id])?;
            match rows.next()? {
                Some(row) => Ok(Some(read_continuation_row(row)?)),
                None => Ok(None),
            }
        })
        .await
    }

    /// Retire every non-retired binding of one conversation (branch
    /// retirement / conversation deletion); rows stay for audit.
    pub async fn retire_continuations(&self, conversation: &str) -> Result<usize> {
        let conversation = conversation.to_owned();
        self.call(move |db| {
            Ok(db.execute(
                "UPDATE native_continuations SET state='retired' WHERE conversation=?1 AND state!='retired'",
                params![conversation],
            )?)
        })
        .await
    }

    /// Record a terminal broker call outcome under its invocation.
    /// Identical redelivery of the same invocation is idempotent; a
    /// different record under one invocation is a hard refusal.
    pub async fn record_call_receipt(&self, receipt: NativeCallReceipt) -> Result<()> {
        if receipt.attempt_id.trim().is_empty()
            || receipt.invocation_id.trim().is_empty()
            || receipt.tool_name.trim().is_empty()
        {
            return Err(StoreError::InvalidInput(
                "call receipt needs attempt, invocation and tool".into(),
            ));
        }
        let expected = receipt.clone();
        self.call(move |db| {
            if let Some(prior) = read_receipt(db, &expected.attempt_id, &expected.invocation_id)? {
                if prior == expected {
                    return Ok(());
                }
                return Err(StoreError::Duplicate(
                    "invocation already carries a different receipt".into(),
                ));
            }
            db.execute(
                "INSERT INTO native_call_receipts VALUES(?,?,?,?,?,?,?,?)",
                params![
                    format!("{}_{}", expected.attempt_id, expected.invocation_id),
                    expected.attempt_id,
                    expected.task_id,
                    expected.invocation_id,
                    expected.tool_name,
                    expected.content_hash,
                    serde_json::to_string(&expected.record)?,
                    expected.recorded_at,
                ],
            )?;
            Ok(())
        })
        .await
    }

    /// The durable receipt for one invocation, if recorded.
    pub async fn get_call_receipt(
        &self,
        attempt_id: &str,
        invocation_id: &str,
    ) -> Result<Option<NativeCallReceipt>> {
        let attempt_id = attempt_id.to_owned();
        let invocation_id = invocation_id.to_owned();
        self.call(move |db| read_receipt(db, &attempt_id, &invocation_id))
            .await
    }
}

type ScopeKey = (
    String,
    String,
    i64,
    String,
    String,
    String,
);

fn find_active(db: &rusqlite::Connection, key: &ScopeKey) -> Result<Vec<NativeContinuationBinding>> {
    let mut statement = db.prepare(
        "SELECT binding_id, conversation, parent, history_position, backend, executable, config_identity, system_prompt_revision, vendor_session_id, owning_attempt_id, owning_task_id, state, committed_at FROM native_continuations WHERE conversation=?1 AND parent=?2 AND history_position=?3 AND backend=?4 AND executable=?5 AND config_identity=?6 AND state='active' ORDER BY committed_at DESC",
    )?;
    let rows = statement.query_map(
        params![key.0, key.1, key.2, key.3, key.4, key.5],
        read_continuation_row,
    )?;
    rows.map(|row| row.map_err(StoreError::from)).collect()
}

fn read_continuation(
    db: &rusqlite::Connection,
    binding_id: &str,
) -> Result<Option<NativeContinuationBinding>> {
    let mut statement = db.prepare(
        "SELECT binding_id, conversation, parent, history_position, backend, executable, config_identity, system_prompt_revision, vendor_session_id, owning_attempt_id, owning_task_id, state, committed_at FROM native_continuations WHERE binding_id=?1",
    )?;
    let mut rows = statement.query(params![binding_id])?;
    match rows.next()? {
        Some(row) => Ok(Some(read_continuation_row(row)?)),
        None => Ok(None),
    }
}

fn read_continuation_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<NativeContinuationBinding> {
    Ok(NativeContinuationBinding {
        binding_id: row.get(0)?,
        conversation: row.get(1)?,
        parent: row.get(2)?,
        history_position: row.get(3)?,
        backend: row.get(4)?,
        executable: row.get(5)?,
        config_identity: row.get(6)?,
        system_prompt_revision: row.get(7)?,
        vendor_session_id: row.get(8)?,
        owning_attempt_id: row.get(9)?,
        owning_task_id: row.get(10)?,
        state: row.get(11)?,
        committed_at: row.get(12)?,
    })
}

fn read_receipt(
    db: &rusqlite::Connection,
    attempt_id: &str,
    invocation_id: &str,
) -> Result<Option<NativeCallReceipt>> {
    let mut statement = db.prepare(
        "SELECT attempt_id, task_id, invocation_id, tool_name, content_hash, record, recorded_at FROM native_call_receipts WHERE attempt_id=?1 AND invocation_id=?2",
    )?;
    let mut rows = statement.query(params![attempt_id, invocation_id])?;
    match rows.next()? {
        Some(row) => {
            let record: String = row.get(5)?;
            Ok(Some(NativeCallReceipt {
                attempt_id: row.get(0)?,
                task_id: row.get(1)?,
                invocation_id: row.get(2)?,
                tool_name: row.get(3)?,
                content_hash: row.get(4)?,
                record: serde_json::from_str(&record)
                    .map_err(|e| rusqlite::Error::InvalidParameterName(e.to_string()))?,
                recorded_at: row.get(6)?,
            }))
        }
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ClaimInput, SettleTask, SqliteStore, SubmitTask, TaskStatus};
    use serde_json::json;

    fn store() -> SqliteStore {
        SqliteStore::memory(64).unwrap()
    }

    async fn submit_task(store: &SqliteStore, task_id: &str) {
        store
            .submit(SubmitTask {
                task_id: task_id.into(),
                kind: "native.turn".into(),
                scope_id: "chat-1".into(),
                payload: json!({}),
                priority: 0,
                now: 1000,
                available_at: None,
                depends_on: vec![],
                parent_task_id: None,
                exclusive_scope: false,
            })
            .await
            .unwrap();
    }

    async fn claim_task(store: &SqliteStore) -> crate::ClaimedTask {
        store
            .claim(ClaimInput {
                owner_id: "worker-1".into(),
                now: 1500,
                ttl_ms: 600_000,
                kinds: vec!["native.turn".into()],
                busy_scopes: vec![],
                max_attempts: 5,
            })
            .await
            .unwrap()
            .expect("task claimable")
    }

    fn commit(task: &str, attempt: &str, session: &str) -> CommitContinuation {
        CommitContinuation {
            conversation: "chat-1".into(),
            parent: "msg-7".into(),
            history_position: 7,
            backend: "claude-code".into(),
            executable: "/usr/bin/claude".into(),
            config_identity: "cfg-aaa".into(),
            system_prompt_revision: Some("sp-1".into()),
            vendor_session_id: session.into(),
            owning_attempt_id: attempt.into(),
            owning_task_id: task.into(),
            committed_at: 2000,
        }
    }

    #[tokio::test]
    async fn commit_is_idempotent_and_scoped() {
        let store = store();
        submit_task(&store, "task-a").await;
        let first = store
            .commit_continuation(commit("task-a", "attempt-a", "sess-1"))
            .await
            .unwrap();
        let again = store
            .commit_continuation(commit("task-a", "attempt-a", "sess-1"))
            .await
            .unwrap();
        assert_eq!(first, again, "identical re-commit is a no-op");
        let found = store
            .active_continuation("chat-1", "msg-7", 7, "claude-code", "/usr/bin/claude", "cfg-aaa")
            .await
            .unwrap()
            .expect("active binding");
        assert_eq!(found.vendor_session_id, "sess-1");
        // Any single scope component differs → miss (no global identity).
        assert!(
            store
                .active_continuation("chat-1", "msg-7", 7, "claude-code", "/usr/bin/claude", "cfg-BBB")
                .await
                .unwrap()
                .is_none(),
            "changed config identity must not resolve the binding"
        );
        assert!(
            store
                .active_continuation("chat-OTHER", "msg-7", 7, "claude-code", "/usr/bin/claude", "cfg-aaa")
                .await
                .unwrap()
                .is_none(),
            "foreign conversation must not resolve the binding"
        );
        assert!(
            store
                .active_continuation("chat-1", "msg-6", 6, "claude-code", "/usr/bin/claude", "cfg-aaa")
                .await
                .unwrap()
                .is_none(),
            "different history position must not resolve the binding"
        );
    }

    #[tokio::test]
    async fn supersession_is_fenced_on_the_prior_owner() {
        let store = store();
        submit_task(&store, "task-a").await;
        submit_task(&store, "task-b").await;
        store
            .commit_continuation(commit("task-a", "attempt-a", "sess-1"))
            .await
            .unwrap();
        // A live claim on the prior owner's task fences the transition.
        let claimed_a = claim_task(&store).await;
        assert_eq!(claimed_a.task.task_id, "task-a");
        let mut next = commit("task-b", "attempt-b", "sess-2");
        next.committed_at = 2100;
        let fenced = store.commit_continuation(next.clone()).await;
        assert!(fenced.is_err(), "live prior owner must block supersession");

        // Settle the prior owner: supersession now succeeds and the prior
        // row is superseded, never active again.
        store
            .settle(SettleTask {
                fence: claimed_a.lease.fence(),
                now: 2200,
                status: TaskStatus::Completed,
                error: None,
                events: vec![],
                messages: vec![],
            })
            .await
            .unwrap();
        let committed = store.commit_continuation(next.clone()).await.unwrap();
        assert_eq!(committed.state, "active");
        assert_eq!(committed.vendor_session_id, "sess-2");
        let prior = store
            .continuation_for_attempt("attempt-a")
            .await
            .unwrap()
            .expect("prior row retained for audit");
        assert_eq!(prior.state, "superseded");
        let active = store
            .active_continuation("chat-1", "msg-7", 7, "claude-code", "/usr/bin/claude", "cfg-aaa")
            .await
            .unwrap()
            .expect("one active binding");
        assert_eq!(active.binding_id, committed.binding_id);
    }

    #[tokio::test]
    async fn receipts_key_on_invocation_not_arguments() {
        let store = store();
        submit_task(&store, "task-a").await;
        let receipt = NativeCallReceipt {
            attempt_id: "attempt-a".into(),
            task_id: "task-a".into(),
            invocation_id: "inv-1".into(),
            tool_name: "note_search".into(),
            content_hash: "hash-1".into(),
            record: json!({"executed": true}),
            recorded_at: 3000,
        };
        store.record_call_receipt(receipt.clone()).await.unwrap();
        store.record_call_receipt(receipt.clone()).await.unwrap();
        let read = store
            .get_call_receipt("attempt-a", "inv-1")
            .await
            .unwrap()
            .expect("receipt");
        assert_eq!(read.record, json!({"executed": true}));
        // Same arguments under a NEW invocation is a new receipt: no
        // collision, no replay of the first.
        let mut other = receipt.clone();
        other.invocation_id = "inv-2".into();
        other.record = json!({"executed": true, "second": true});
        store.record_call_receipt(other).await.unwrap();
        assert_eq!(
            store
                .get_call_receipt("attempt-a", "inv-2")
                .await
                .unwrap()
                .unwrap()
                .record,
            json!({"executed": true, "second": true})
        );
        // A different record under ONE invocation is refused.
        let mut conflict = receipt.clone();
        conflict.record = json!({"executed": false});
        assert!(store.record_call_receipt(conflict).await.is_err());
    }
}
