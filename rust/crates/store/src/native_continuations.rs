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

use agentkit_native::{CommitContinuation, NativeContinuationBinding, canonical_digest};

use crate::{Result, StoreError};

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
            claimed_attempt_id: None,
            claimed_task_id: None,
            claimed_at: None,
            generation: 0,
        };
        let expected = binding.clone();
        self.call(move |db| {
            let transaction = crate::transaction::atomic(db)?;
            if let Some(existing) = read_continuation(&transaction, &expected.binding_id)? {
                // Idempotent re-commit by the owning attempt only. The
                // frozen facts (everything the COMMIT attested) decide
                // identity; claim columns and generation are ledger
                // bookkeeping around them, never commit facts.
                if frozen_facts(&existing) == frozen_facts(&expected) {
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
            // A NEW binding is a durable authority claim: the committing
            // attempt must still hold current authority over its task in
            // this very transaction (AGENTKIT-115) — a cancelled,
            // lease-stripped or expired attempt can never mint an active,
            // resumable binding (review §2.3; a later rejected event cannot
            // undo what an unauthorized commit already published).
            if let Some(gap) =
                crate::native_exec::live_authority_gap(&transaction, &expected.owning_task_id, &expected.owning_attempt_id, expected.committed_at)?
            {
                return Err(StoreError::InvalidInput(format!(
                    "continuation commit refused: owning attempt lacks current authority ({gap})"
                )));
            }
            // Supersede prior non-terminal bindings on the same scope key —
            // fenced on the prior owner having settled (a claim an
            // interrupted attempt left `owned`/`uncertain` is retired here
            // too, not only successful completions — review round-4 F3-A:
            // retirement of older positions must cover interrupted
            // attempts).
            let scope_key = (
                expected.conversation.clone(),
                expected.parent.clone(),
                expected.history_position,
                expected.backend.clone(),
                expected.executable.clone(),
                expected.config_identity.clone(),
                expected.system_prompt_revision.clone(),
            );
            let prior = find_open(&transaction, &scope_key)?;
            for prior in prior
                .into_iter()
                .filter(|prior| prior.binding_id != expected.binding_id)
            {
                retire_prior(&transaction, &prior, &expected.owning_task_id)?;
            }
            // A vendor session is a MUTABLE cursor: advancing it to a new
            // conversation position retires every OTHER open alias to the
            // same session (older input positions must never remain
            // resumable alongside the advanced one — review §2.3; and an
            // interrupted attempt's claimed/uncertain alias is retired by
            // this sweep too, review round-4 F3-A).
            let aliases = {
                let mut statement = transaction.prepare(
                    "SELECT binding_id, conversation, parent, history_position, backend, executable, config_identity, system_prompt_revision, vendor_session_id, owning_attempt_id, owning_task_id, state, committed_at, claimed_attempt_id, claimed_task_id, claimed_at, generation FROM native_continuations WHERE conversation=?1 AND vendor_session_id=?2 AND state IN ('active','owned','uncertain') AND binding_id!=?3",
                )?;
                let rows = statement.query_map(
                    params![expected.conversation, expected.vendor_session_id, expected.binding_id],
                    read_continuation_row,
                )?;
                rows.map(|row| row.map_err(StoreError::from)).collect::<Result<Vec<_>>>()?
            };
            for alias in aliases {
                retire_prior(&transaction, &alias, &expected.owning_task_id)?;
            }
            transaction.execute(
                "INSERT INTO native_continuations VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
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
                    expected.claimed_attempt_id,
                    expected.claimed_task_id,
                    expected.claimed_at,
                    expected.generation,
                ],
            )?;
            transaction.commit()?;
            Ok(expected)
        })
        .await
    }

    /// The active binding for one exact scope, if any. Every component
    /// must match — including the system-prompt revision: a changed prompt
    /// is a changed configuration and therefore a MISS (the caller chooses
    /// Fresh with seeded ancestry). This is what an exact resume must
    /// prove; a miss is a miss.
    #[allow(clippy::too_many_arguments)]
    pub async fn active_continuation(
        &self,
        conversation: &str,
        parent: &str,
        history_position: i64,
        backend: &str,
        executable: &str,
        config_identity: &str,
        system_prompt_revision: Option<&str>,
    ) -> Result<Option<NativeContinuationBinding>> {
        let (conversation, parent, backend, executable, config_identity) = (
            conversation.to_owned(),
            parent.to_owned(),
            backend.to_owned(),
            executable.to_owned(),
            config_identity.to_owned(),
        );
        let system_prompt_revision = system_prompt_revision.map(str::to_owned);
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
                    system_prompt_revision,
                ),
            )
            .map(|rows| rows.into_iter().next())
        })
        .await
    }

    /// Atomically reserve the binding at one exact scope for ONE attempt
    /// (review round-4 F3): `active → owned` as a generation-keyed
    /// compare-and-set inside a single transaction, fenced on the
    /// claimant holding CURRENT lease authority over its task. Exactly one
    /// concurrent claim wins (`Ok(Some)`); every loser observes `Ok(None)`
    /// and must take the fresh-session path — the primitive behind
    /// OneMind's `exclusive_scope` is repaired here, not assumed (F3-C).
    /// Re-entry by the attempt that already holds the claim is idempotent.
    ///
    /// The claim is what makes an interrupted attempt's input position
    /// non-resumable: once `owned`, no OTHER lookup may select the binding
    /// (the vendor session may already have consumed this attempt's
    /// input), and after an interruption the claimant marks it
    /// `uncertain` so even exact re-resume from the pre-claim position is
    /// refused (F3-A).
    #[allow(clippy::too_many_arguments)]
    pub async fn claim_continuation(
        &self,
        conversation: &str,
        parent: &str,
        history_position: i64,
        backend: &str,
        executable: &str,
        config_identity: &str,
        system_prompt_revision: Option<&str>,
        claimant_task_id: &str,
        claimant_attempt_id: &str,
        now: i64,
    ) -> Result<Option<NativeContinuationBinding>> {
        if conversation.trim().is_empty()
            || parent.trim().is_empty()
            || backend.trim().is_empty()
            || executable.trim().is_empty()
            || config_identity.trim().is_empty()
            || claimant_task_id.trim().is_empty()
            || claimant_attempt_id.trim().is_empty()
            || history_position < 0
        {
            return Err(StoreError::InvalidInput(
                "continuation claim needs full scope and claimant".into(),
            ));
        }
        let selected = self
            .resumable_continuation(
                conversation,
                parent,
                history_position,
                backend,
                executable,
                config_identity,
                system_prompt_revision,
                claimant_attempt_id,
            )
            .await?;
        let Some(selected) = selected else {
            let task = claimant_task_id.to_owned();
            let attempt = claimant_attempt_id.to_owned();
            return self.call(move |db| {
                if let Some(gap) = crate::native_exec::live_authority_gap(db, &task, &attempt, now)? {
                    return Err(StoreError::InvalidInput(format!(
                        "continuation claim refused: claiming attempt lacks current authority ({gap})"
                    )));
                }
                Ok(None)
            }).await;
        };
        self.claim_selected_continuation(&selected, claimant_task_id, claimant_attempt_id, now)
            .await
    }

    /// Claim only the exact binding selected before asynchronous preflight.
    /// A changed generation, session, or current scope candidate is a miss;
    /// selecting again here could reserve a different vendor session.
    pub async fn claim_selected_continuation(
        &self,
        selected: &NativeContinuationBinding,
        claimant_task_id: &str,
        claimant_attempt_id: &str,
        now: i64,
    ) -> Result<Option<NativeContinuationBinding>> {
        if selected.conversation.trim().is_empty()
            || selected.parent.trim().is_empty()
            || selected.backend.trim().is_empty()
            || selected.executable.trim().is_empty()
            || selected.config_identity.trim().is_empty()
            || claimant_task_id.trim().is_empty()
            || claimant_attempt_id.trim().is_empty()
            || selected.history_position < 0
        {
            return Err(StoreError::InvalidInput(
                "continuation claim needs full scope and claimant".into(),
            ));
        }
        let key: ScopeKey = (
            selected.conversation.clone(),
            selected.parent.clone(),
            selected.history_position,
            selected.backend.clone(),
            selected.executable.clone(),
            selected.config_identity.clone(),
            selected.system_prompt_revision.clone(),
        );
        let (claimant_task_id, claimant_attempt_id) =
            (claimant_task_id.to_owned(), claimant_attempt_id.to_owned());
        let selected = selected.clone();
        self.call(move |db| {
            let transaction = crate::transaction::atomic(db)?;
            // Lease-fenced: an attempt without CURRENT authority cannot
            // reserve anything (same bar as a commit — AGENTKIT-115).
            if let Some(gap) = crate::native_exec::live_authority_gap(
                &transaction,
                &claimant_task_id,
                &claimant_attempt_id,
                now,
            )? {
                return Err(StoreError::InvalidInput(format!(
                    "continuation claim refused: claiming attempt lacks current authority ({gap})"
                )));
            }
            let candidates = find_resumable(&transaction, &key, &claimant_attempt_id)?;
            let Some(candidate) = candidates.into_iter().next() else {
                transaction.commit()?;
                return Ok(None);
            };
            if candidate.binding_id != selected.binding_id
                || candidate.generation != selected.generation
                || candidate.vendor_session_id != selected.vendor_session_id
            {
                transaction.commit()?;
                return Ok(None);
            }
            if candidate.state == "owned" {
                // Re-entry: this attempt already holds the claim (the
                // only `owned` row `find_resumable` returns).
                transaction.commit()?;
                return Ok(Some(candidate));
            }
            let changed = transaction.execute(
                "UPDATE native_continuations SET state='owned', claimed_attempt_id=?2, claimed_task_id=?3, claimed_at=?4, generation=generation+1 WHERE binding_id=?1 AND state='active' AND generation=?5",
                params![
                    candidate.binding_id,
                    claimant_attempt_id,
                    claimant_task_id,
                    now,
                    candidate.generation,
                ],
            )?;
            if changed == 0 {
                // Lost the race to a concurrent claim: exactly one wins.
                transaction.commit()?;
                return Ok(None);
            }
            let claimed = read_continuation(&transaction, &candidate.binding_id)?;
            transaction.commit()?;
            Ok(claimed)
        })
        .await
    }

    /// The exact-resume view for ONE attempt: the binding at the scope is
    /// still available, or `owned` by this very attempt (claim re-entry).
    /// Anything else — superseded, retired, or owned by another attempt —
    /// is a miss: the observed-session shortcut must be validated against
    /// this before it may reuse an identity (F3-B), and a claim never
    /// leaks to a second consumer (F3-C).
    #[allow(clippy::too_many_arguments)]
    pub async fn resumable_continuation(
        &self,
        conversation: &str,
        parent: &str,
        history_position: i64,
        backend: &str,
        executable: &str,
        config_identity: &str,
        system_prompt_revision: Option<&str>,
        attempt_id: &str,
    ) -> Result<Option<NativeContinuationBinding>> {
        let key: ScopeKey = (
            conversation.to_owned(),
            parent.to_owned(),
            history_position,
            backend.to_owned(),
            executable.to_owned(),
            config_identity.to_owned(),
            system_prompt_revision.map(str::to_owned),
        );
        let attempt_id = attempt_id.to_owned();
        self.call(move |db| {
            find_resumable(db, &key, &attempt_id).map(|rows| rows.into_iter().next())
        })
        .await
    }

    /// Prove the input was NOT accepted and give the position back:
    /// `owned → active`, claimant-cleared, generation bumped. Only the
    /// claimant (or a settled re-entry of its attempt) may release what it
    /// reserved — an unclaimed/foreign row is a no-op, never a takeover.
    pub async fn release_continuation_claim(
        &self,
        binding_id: &str,
        claimant_attempt_id: &str,
    ) -> Result<Option<NativeContinuationBinding>> {
        let (binding_id, claimant_attempt_id) =
            (binding_id.to_owned(), claimant_attempt_id.to_owned());
        self.call(move |db| {
            let transaction = crate::transaction::atomic(db)?;
            let changed = transaction.execute(
                "UPDATE native_continuations SET state='active', claimed_attempt_id=NULL, claimed_task_id=NULL, claimed_at=NULL, generation=generation+1 WHERE binding_id=?1 AND state='owned' AND claimed_attempt_id=?2",
                params![binding_id, claimant_attempt_id],
            )?;
            let outcome = if changed > 0 {
                read_continuation(&transaction, &binding_id)?
            } else {
                // Already released (replay) — an AVAILABLE row is the
                // desired end state, so restating is fine; anything else
                // (superseded/retired/foreign) is honestly "not yours".
                let existing = read_continuation(&transaction, &binding_id)?;
                existing.filter(|row| row.state == "active")
            };
            transaction.commit()?;
            Ok(outcome)
        })
        .await
    }

    /// The attempt was interrupted AFTER its input was accepted: the
    /// vendor session may have consumed the prompt (and part of the
    /// answer), so the pre-claim position must never present as available
    /// again. `owned → uncertain` blocks exact resume from it until an
    /// explicit reconcile or until the conversation moves on through a
    /// fresh session (review round-4 F3-A). Claimant-fenced like release.
    pub async fn mark_continuation_uncertain(
        &self,
        binding_id: &str,
        claimant_attempt_id: &str,
    ) -> Result<Option<NativeContinuationBinding>> {
        let (binding_id, claimant_attempt_id) =
            (binding_id.to_owned(), claimant_attempt_id.to_owned());
        self.call(move |db| {
            let transaction = crate::transaction::atomic(db)?;
            let changed = transaction.execute(
                "UPDATE native_continuations SET state='uncertain', generation=generation+1 WHERE binding_id=?1 AND state='owned' AND claimed_attempt_id=?2",
                params![binding_id, claimant_attempt_id],
            )?;
            let outcome = if changed > 0 {
                read_continuation(&transaction, &binding_id)?
            } else {
                // Idempotent restatement: already uncertain is the goal.
                let existing = read_continuation(&transaction, &binding_id)?;
                existing.filter(|row| row.state == "uncertain")
            };
            transaction.commit()?;
            Ok(outcome)
        })
        .await
    }

    /// Reconcile an uncertain binding back to AVAILABLE (the consumer has
    /// proven the vendor position honest, e.g. the interrupted turn's
    /// outcome was actually delivered): `uncertain → active`,
    /// claimant-cleared, generation bumped. This is the ONLY edge that
    /// returns an uncertain row to service; the alternative — a fresh
    /// session — simply never reconciles and lets the commit sweep
    /// supersede the row.
    pub async fn reconcile_continuation(
        &self,
        binding_id: &str,
    ) -> Result<Option<NativeContinuationBinding>> {
        let binding_id = binding_id.to_owned();
        self.call(move |db| {
            let transaction = crate::transaction::atomic(db)?;
            let changed = transaction.execute(
                "UPDATE native_continuations SET state='active', claimed_attempt_id=NULL, claimed_task_id=NULL, claimed_at=NULL, generation=generation+1 WHERE binding_id=?1 AND state='uncertain'",
                params![binding_id],
            )?;
            let outcome = if changed > 0 {
                read_continuation(&transaction, &binding_id)?
            } else {
                let existing = read_continuation(&transaction, &binding_id)?;
                existing.filter(|row| row.state == "active")
            };
            transaction.commit()?;
            Ok(outcome)
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
                "SELECT binding_id, conversation, parent, history_position, backend, executable, config_identity, system_prompt_revision, vendor_session_id, owning_attempt_id, owning_task_id, state, committed_at, claimed_attempt_id, claimed_task_id, claimed_at, generation FROM native_continuations WHERE owning_attempt_id=?1 ORDER BY committed_at DESC LIMIT 1",
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

    /// Persist an effect-start or terminal transition before acknowledging it.
    /// A crash leaving effectStarted is durable evidence of an uncertain effect.
    /// Identity never changes, and an uncertain cleanup cannot overwrite a
    /// conclusive terminal receipt that won the race.
    pub async fn record_call_lifecycle(&self, receipt: NativeCallReceipt) -> Result<()> {
        if serde_json::to_vec(&receipt)?.len() > 2 * 1024 * 1024 {
            return Err(StoreError::InvalidInput(
                "call receipt exceeds 2 MiB".into(),
            ));
        }
        let phase = receipt
            .record
            .get("lifecycleState")
            .and_then(serde_json::Value::as_str);
        if !matches!(phase, Some("effectStarted" | "terminal" | "uncertain"))
            || receipt.attempt_id.is_empty()
            || receipt.invocation_id.is_empty()
            || receipt.tool_name.is_empty()
            || receipt.content_hash.is_empty()
        {
            return Err(StoreError::InvalidInput(
                "invalid call lifecycle identity or phase".into(),
            ));
        }
        self.call(move |db| {
            let transaction = crate::transaction::atomic(db)?;
            if let Some(prior) = read_receipt(&transaction, &receipt.attempt_id, &receipt.invocation_id)? {
                if prior.task_id != receipt.task_id || prior.tool_name != receipt.tool_name
                    || prior.content_hash != receipt.content_hash
                {
                    return Err(StoreError::InvalidInput("call lifecycle identity changed".into()));
                }
                if prior.record == receipt.record
                    || (prior.record["lifecycleState"] == "terminal" && receipt.record["lifecycleState"] == "uncertain") {
                    transaction.commit()?;
                    return Ok(());
                }
                if prior.record["lifecycleState"] != "effectStarted" {
                    return Err(StoreError::Duplicate("call lifecycle already settled".into()));
                }
                transaction.execute("UPDATE native_call_receipts SET record=?3, recorded_at=?4 WHERE attempt_id=?1 AND invocation_id=?2",
                    params![receipt.attempt_id, receipt.invocation_id, serde_json::to_string(&receipt.record)?, receipt.recorded_at])?;
            } else {
                if receipt.record["lifecycleState"] == "effectStarted"
                    && let Some(gap) = crate::native_exec::live_authority_gap(&transaction, &receipt.task_id, &receipt.attempt_id, receipt.recorded_at)?
                {
                    return Err(StoreError::InvalidInput(format!("effect start lacks live authority: {gap}")));
                }
                transaction.execute("INSERT INTO native_call_receipts VALUES(?,?,?,?,?,?,?,?)",
                    params![format!("{}_{}", receipt.attempt_id, receipt.invocation_id), receipt.attempt_id, receipt.task_id, receipt.invocation_id,
                        receipt.tool_name, receipt.content_hash, serde_json::to_string(&receipt.record)?, receipt.recorded_at])?;
            }
            transaction.commit()?;
            Ok(())
        }).await
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

type ScopeKey = (String, String, i64, String, String, String, Option<String>);

/// Supersede one prior non-terminal binding, fenced on the prior owner
/// having SETTLED: a live prior owner is a hard refusal, never a silent
/// handover. `by_task` is the task performing the transition: retiring
/// one's OWN live claim/alias (the completing attempt consuming the
/// input-position binding it claimed, or the same task's stale alias) is
/// never fenced against itself. Covers claims an interrupted attempt left
/// behind (`owned`/`uncertain`), not just active rows — review round-4
/// F3-A: retirement of older positions must cover interrupted attempts.
fn retire_prior(
    transaction: &rusqlite::Connection,
    prior: &NativeContinuationBinding,
    by_task: &str,
) -> Result<()> {
    if prior.owning_task_id == by_task || prior.claimed_task_id.as_deref() == Some(by_task) {
        transaction.execute(
            "UPDATE native_continuations SET state='superseded', generation=generation+1 WHERE binding_id=?1 AND state IN ('active','owned','uncertain')",
            params![prior.binding_id],
        )?;
        return Ok(());
    }
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
        "UPDATE native_continuations SET state='superseded', generation=generation+1 WHERE binding_id=?1 AND state IN ('active','owned','uncertain')",
        params![prior.binding_id],
    )?;
    Ok(())
}

/// The COMMIT facts a binding attests — everything except the claim
/// columns and generation, which are ledger bookkeeping around the
/// commit, never part of the frozen record an idempotent re-commit must
/// restate.
fn frozen_facts(binding: &NativeContinuationBinding) -> NativeContinuationBinding {
    let mut facts = binding.clone();
    facts.claimed_attempt_id = None;
    facts.claimed_task_id = None;
    facts.claimed_at = None;
    facts.generation = 0;
    facts
}

fn find_active(
    db: &rusqlite::Connection,
    key: &ScopeKey,
) -> Result<Vec<NativeContinuationBinding>> {
    find_in_states(db, key, "state='active'", None)
}

/// Non-terminal rows on one scope key (available, claimed, or uncertain) —
/// what a NEW commit must supersede before publishing its position.
fn find_open(db: &rusqlite::Connection, key: &ScopeKey) -> Result<Vec<NativeContinuationBinding>> {
    find_in_states(db, key, "state IN ('active','owned','uncertain')", None)
}

/// Rows on one scope key an EXACT resume may proceed from for `attempt`:
/// still available, or `owned` by this very attempt (a re-entry after a
/// persistence failure restates its own claim — never another attempt's).
fn find_resumable(
    db: &rusqlite::Connection,
    key: &ScopeKey,
    attempt: &str,
) -> Result<Vec<NativeContinuationBinding>> {
    find_in_states(
        db,
        key,
        "(state='active' OR (state='owned' AND claimed_attempt_id=?8))",
        Some(attempt),
    )
}

fn find_in_states(
    db: &rusqlite::Connection,
    key: &ScopeKey,
    state_filter: &str,
    claimant: Option<&str>,
) -> Result<Vec<NativeContinuationBinding>> {
    let sql = format!(
        "SELECT binding_id, conversation, parent, history_position, backend, executable, config_identity, system_prompt_revision, vendor_session_id, owning_attempt_id, owning_task_id, state, committed_at, claimed_attempt_id, claimed_task_id, claimed_at, generation FROM native_continuations WHERE conversation=?1 AND parent=?2 AND history_position=?3 AND backend=?4 AND executable=?5 AND config_identity=?6 AND system_prompt_revision IS ?7 AND {state_filter} ORDER BY committed_at DESC"
    );
    let mut statement = db.prepare(&sql)?;
    let rows = match claimant {
        Some(attempt) => statement.query_map(
            params![key.0, key.1, key.2, key.3, key.4, key.5, key.6, attempt],
            read_continuation_row,
        )?,
        None => statement.query_map(
            params![key.0, key.1, key.2, key.3, key.4, key.5, key.6],
            read_continuation_row,
        )?,
    };
    rows.map(|row| row.map_err(StoreError::from)).collect()
}

fn read_continuation(
    db: &rusqlite::Connection,
    binding_id: &str,
) -> Result<Option<NativeContinuationBinding>> {
    let mut statement = db.prepare(
        "SELECT binding_id, conversation, parent, history_position, backend, executable, config_identity, system_prompt_revision, vendor_session_id, owning_attempt_id, owning_task_id, state, committed_at, claimed_attempt_id, claimed_task_id, claimed_at, generation FROM native_continuations WHERE binding_id=?1",
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
        claimed_attempt_id: row.get(13)?,
        claimed_task_id: row.get(14)?,
        claimed_at: row.get(15)?,
        generation: row.get(16)?,
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

    /// Submit + claim so the owning task is RUNNING under a live lease:
    /// a durable continuation commit requires current owner authority
    /// (AGENTKIT-115), exactly like the executor's settled path. Distinct
    /// `scope_id`s keep concurrent owner tasks out of each other's
    /// execution lane; the continuation `conversation` is opaque scope
    /// identity the store never derives from the task scope.
    async fn running_task(store: &SqliteStore, task_id: &str, scope_id: &str) -> String {
        store
            .submit(SubmitTask {
                task_id: task_id.into(),
                kind: "native.turn".into(),
                scope_id: scope_id.into(),
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
        let claimed = claim_task(store).await;
        assert_eq!(claimed.task.task_id, task_id);
        claimed.lease.attempt_id
    }

    #[tokio::test]
    async fn commit_is_idempotent_and_scoped() {
        let store = store();
        let attempt_a = running_task(&store, "task-a", "chat-turn-a").await;
        let first = store
            .commit_continuation(commit("task-a", &attempt_a, "sess-1"))
            .await
            .unwrap();
        let again = store
            .commit_continuation(commit("task-a", &attempt_a, "sess-1"))
            .await
            .unwrap();
        assert_eq!(first, again, "identical re-commit is a no-op");
        let found = store
            .active_continuation(
                "chat-1",
                "msg-7",
                7,
                "claude-code",
                "/usr/bin/claude",
                "cfg-aaa",
                Some("sp-1"),
            )
            .await
            .unwrap()
            .expect("active binding");
        assert_eq!(found.vendor_session_id, "sess-1");
        // Any single scope component differs → miss (no global identity).
        assert!(
            store
                .active_continuation(
                    "chat-1",
                    "msg-7",
                    7,
                    "claude-code",
                    "/usr/bin/claude",
                    "cfg-BBB",
                    Some("sp-1")
                )
                .await
                .unwrap()
                .is_none(),
            "changed config identity must not resolve the binding"
        );
        assert!(
            store
                .active_continuation(
                    "chat-OTHER",
                    "msg-7",
                    7,
                    "claude-code",
                    "/usr/bin/claude",
                    "cfg-aaa",
                    Some("sp-1")
                )
                .await
                .unwrap()
                .is_none(),
            "foreign conversation must not resolve the binding"
        );
        assert!(
            store
                .active_continuation(
                    "chat-1",
                    "msg-6",
                    6,
                    "claude-code",
                    "/usr/bin/claude",
                    "cfg-aaa",
                    Some("sp-1")
                )
                .await
                .unwrap()
                .is_none(),
            "different history position must not resolve the binding"
        );
        // The system-prompt revision is part of the active lookup: a
        // changed (or absent) prompt is a MISS, never a silent match
        // (review §2.3 configuration matching).
        assert!(
            store
                .active_continuation(
                    "chat-1",
                    "msg-7",
                    7,
                    "claude-code",
                    "/usr/bin/claude",
                    "cfg-aaa",
                    Some("sp-2")
                )
                .await
                .unwrap()
                .is_none(),
            "changed prompt revision must not resolve the binding"
        );
        assert!(
            store
                .active_continuation(
                    "chat-1",
                    "msg-7",
                    7,
                    "claude-code",
                    "/usr/bin/claude",
                    "cfg-aaa",
                    None
                )
                .await
                .unwrap()
                .is_none(),
            "absent prompt revision must not match a committed prompt"
        );
    }

    #[tokio::test]
    async fn commit_requires_current_owner_authority() {
        // Review §2.3 reproduced: a continuation committed after the task
        // was cancelled and its lease removed must NOT create an active,
        // resumable binding.
        let store = store();
        let attempt_a = running_task(&store, "task-a", "chat-turn-a").await;
        store.cancel_task("task-a", 1600).await.unwrap();
        let err = store
            .commit_continuation(commit("task-a", &attempt_a, "sess-ghost"))
            .await
            .expect_err("cancelled, lease-stripped attempt has no authority");
        assert!(
            err.to_string().contains("lacks current authority"),
            "explicit refusal: {err}"
        );
        assert!(
            store
                .active_continuation(
                    "chat-1",
                    "msg-7",
                    7,
                    "claude-code",
                    "/usr/bin/claude",
                    "cfg-aaa",
                    Some("sp-1")
                )
                .await
                .unwrap()
                .is_none(),
            "no active binding may exist from an unauthorized commit"
        );

        // Expired lease alone is also a refusal.
        let store = SqliteStore::memory(64).unwrap();
        submit_task(&store, "task-e").await;
        let claimed = store
            .claim(ClaimInput {
                owner_id: "worker-1".into(),
                now: 1500,
                ttl_ms: 100,
                kinds: vec!["native.turn".into()],
                busy_scopes: vec![],
                max_attempts: 5,
            })
            .await
            .unwrap()
            .expect("claimable");
        let mut late = commit("task-e", &claimed.lease.attempt_id, "sess-late");
        late.committed_at = 9_000; // far past the 1500+100 lease expiry
        assert!(
            store
                .commit_continuation(late)
                .await
                .expect_err("expired lease has no authority")
                .to_string()
                .contains("lease-expired")
        );
        // An attempt the lease does not name is a refusal too.
        let store = SqliteStore::memory(64).unwrap();
        running_task(&store, "task-f", "chat-turn-f").await;
        assert!(
            store
                .commit_continuation(commit("task-f", "att-stranger", "sess-x"))
                .await
                .expect_err("lease names a different attempt")
                .to_string()
                .contains("lease-attempt")
        );
    }

    #[tokio::test]
    async fn advancing_a_session_retires_older_aliases() {
        // Review §2.3 reproduced: advancing the SAME mutable vendor session
        // to a new conversation position must retire the older alias —
        // the old position may never remain resumable alongside it.
        let store = store();
        let attempt_a = running_task(&store, "task-a", "chat-turn-a").await;
        let first = store
            .commit_continuation(commit("task-a", &attempt_a, "sess-1"))
            .await
            .unwrap();
        assert_eq!(first.state, "active");
        // Settle the first owner, then advance the session to a newer
        // position under a second running task.
        let lease_a = store
            .get_lease("task-a")
            .await
            .unwrap()
            .expect("first lease");
        store
            .settle(SettleTask {
                fence: lease_a.fence(),
                now: 2150,
                status: TaskStatus::Completed,
                error: None,
                events: vec![],
                messages: vec![],
            })
            .await
            .unwrap();
        let attempt_b = running_task(&store, "task-b", "chat-turn-b").await;
        let mut advanced = commit("task-b", &attempt_b, "sess-1");
        advanced.parent = "msg-8".into();
        advanced.history_position = 8;
        advanced.committed_at = 2200;
        let committed = store
            .commit_continuation(advanced)
            .await
            .expect("advance commits under live authority");
        assert_eq!(committed.vendor_session_id, "sess-1");
        // The advanced position resolves…
        assert!(
            store
                .active_continuation(
                    "chat-1",
                    "msg-8",
                    8,
                    "claude-code",
                    "/usr/bin/claude",
                    "cfg-aaa",
                    Some("sp-1")
                )
                .await
                .unwrap()
                .is_some(),
            "the advanced position is resumable"
        );
        // …and the OLDER alias to the same mutable session is gone.
        assert!(
            store
                .active_continuation(
                    "chat-1",
                    "msg-7",
                    7,
                    "claude-code",
                    "/usr/bin/claude",
                    "cfg-aaa",
                    Some("sp-1")
                )
                .await
                .unwrap()
                .is_none(),
            "older alias must not remain resumable"
        );
        let prior = store
            .continuation_for_attempt(&attempt_a)
            .await
            .unwrap()
            .expect("prior row kept for audit");
        assert_eq!(prior.state, "superseded");
    }

    #[tokio::test]
    async fn supersession_is_fenced_on_the_prior_owner() {
        let store = store();
        let attempt_a = running_task(&store, "task-a", "chat-turn-a").await;
        store
            .commit_continuation(commit("task-a", &attempt_a, "sess-1"))
            .await
            .unwrap();
        // The prior owner's task is RUNNING with a live lease: the
        // transition into the same scope key is fenced.
        let attempt_b = running_task(&store, "task-b", "chat-turn-b").await;
        let mut next = commit("task-b", &attempt_b, "sess-2");
        next.committed_at = 2100;
        let fenced = store.commit_continuation(next.clone()).await;
        assert!(fenced.is_err(), "live prior owner must block supersession");

        // Settle the prior owner: supersession now succeeds and the prior
        // row is superseded, never active again.
        let lease = store
            .get_lease("task-a")
            .await
            .unwrap()
            .expect("prior lease still present");
        store
            .settle(SettleTask {
                fence: lease.fence(),
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
            .continuation_for_attempt(&attempt_a)
            .await
            .unwrap()
            .expect("prior row retained for audit");
        assert_eq!(prior.state, "superseded");
        let active = store
            .active_continuation(
                "chat-1",
                "msg-7",
                7,
                "claude-code",
                "/usr/bin/claude",
                "cfg-aaa",
                Some("sp-1"),
            )
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

    /// Publish one active binding at the fixture scope under a live task,
    /// then settle that committer so the position is claimable by others.
    async fn published_position(store: &SqliteStore) -> NativeContinuationBinding {
        let committer = running_task(store, "task-c", "chat-turn-c").await;
        let binding = store
            .commit_continuation(commit("task-c", &committer, "sess-1"))
            .await
            .unwrap();
        let lease = store
            .get_lease("task-c")
            .await
            .unwrap()
            .expect("committer lease");
        store
            .settle(SettleTask {
                fence: lease.fence(),
                now: 2150,
                status: TaskStatus::Completed,
                error: None,
                events: vec![],
                messages: vec![],
            })
            .await
            .unwrap();
        binding
    }

    #[allow(clippy::too_many_arguments)]
    async fn claim(
        store: &SqliteStore,
        claimant_task: &str,
        claimant_attempt: &str,
        now: i64,
    ) -> crate::Result<Option<NativeContinuationBinding>> {
        store
            .claim_continuation(
                "chat-1",
                "msg-7",
                7,
                "claude-code",
                "/usr/bin/claude",
                "cfg-aaa",
                Some("sp-1"),
                claimant_task,
                claimant_attempt,
                now,
            )
            .await
    }

    #[tokio::test]
    async fn selected_claim_refuses_changed_generation_and_identity() {
        let store = store();
        let selected = published_position(&store).await;
        let attempt = running_task(&store, "task-a", "chat-turn-a").await;
        {
            let mut stale = selected.clone();
            stale.vendor_session_id = "different-session".into();
            assert!(
                store
                    .claim_selected_continuation(&stale, "task-a", &attempt, 2200)
                    .await
                    .unwrap()
                    .is_none()
            );
        }
        let mut stale = selected.clone();
        stale.generation += 1;
        assert!(
            store
                .claim_selected_continuation(&stale, "task-a", &attempt, 2200)
                .await
                .unwrap()
                .is_none()
        );
        stale = selected.clone();
        stale.binding_id = "different-binding".into();
        assert!(
            store
                .claim_selected_continuation(&stale, "task-a", &attempt, 2200)
                .await
                .unwrap()
                .is_none()
        );
        let claimed = store
            .claim_selected_continuation(&selected, "task-a", &attempt, 2200)
            .await
            .unwrap()
            .unwrap();
        store
            .release_continuation_claim(&claimed.binding_id, &attempt)
            .await
            .unwrap();
        assert!(
            store
                .claim_selected_continuation(&selected, "task-a", &attempt, 2201)
                .await
                .unwrap()
                .is_none(),
            "release invalidates the selected generation"
        );
    }

    #[tokio::test]
    async fn claim_is_atomic_single_winner_and_lease_fenced() {
        // Review round-4 F3-C reproduced: two current tasks for one
        // conversation position must never BOTH start from the same exact
        // binding — the compare-and-set has exactly one winner, the loser
        // gets `None` (fresh-session decision), and a claim without a
        // current lease is a hard refusal.
        let store = store();
        let binding = published_position(&store).await;
        let attempt_a = running_task(&store, "task-a", "chat-turn-a").await;
        let attempt_b = running_task(&store, "task-b", "chat-turn-b").await;

        let won = claim(&store, "task-a", &attempt_a, 2200)
            .await
            .unwrap()
            .expect("first claim wins");
        assert_eq!(won.binding_id, binding.binding_id);
        assert_eq!(won.state, "owned");
        assert_eq!(won.claimed_attempt_id.as_deref(), Some(attempt_a.as_str()));
        assert_eq!(won.claimed_task_id.as_deref(), Some("task-a"));
        assert_eq!(won.generation, binding.generation + 1);

        // While owned, the position is NOT available to any lookup…
        assert!(
            store
                .active_continuation(
                    "chat-1",
                    "msg-7",
                    7,
                    "claude-code",
                    "/usr/bin/claude",
                    "cfg-aaa",
                    Some("sp-1")
                )
                .await
                .unwrap()
                .is_none()
        );
        // …and only the claimant sees it as resumable.
        assert!(
            store
                .resumable_continuation(
                    "chat-1",
                    "msg-7",
                    7,
                    "claude-code",
                    "/usr/bin/claude",
                    "cfg-aaa",
                    Some("sp-1"),
                    &attempt_a,
                )
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            store
                .resumable_continuation(
                    "chat-1",
                    "msg-7",
                    7,
                    "claude-code",
                    "/usr/bin/claude",
                    "cfg-aaa",
                    Some("sp-1"),
                    "att-stranger",
                )
                .await
                .unwrap()
                .is_none()
        );

        // Re-entry by the SAME attempt restates its own claim.
        let again = claim(&store, "task-a", &attempt_a, 2210)
            .await
            .unwrap()
            .expect("claim re-entry is idempotent");
        assert_eq!(again, won);

        // The second task loses the race — it must not consume the binding.
        assert!(
            claim(&store, "task-b", &attempt_b, 2220)
                .await
                .unwrap()
                .is_none()
        );
        // …and the loser can neither release nor mark the winner's claim.
        assert!(
            store
                .release_continuation_claim(&won.binding_id, &attempt_b)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .mark_continuation_uncertain(&won.binding_id, &attempt_b)
                .await
                .unwrap()
                .is_none()
        );

        // An attempt the lease does not name cannot claim at all.
        assert!(
            claim(&store, "task-b", "att-stranger", 2230)
                .await
                .expect_err("lease names a different attempt")
                .to_string()
                .contains("lease-attempt")
        );
    }

    #[tokio::test]
    async fn release_returns_the_position_and_reentry_restates() {
        // Proving the input was NOT accepted gives the position back:
        // owned → active, claimant-cleared; the next claimant wins cleanly.
        let store = store();
        let _binding = published_position(&store).await;
        let attempt_a = running_task(&store, "task-a", "chat-turn-a").await;
        let attempt_b = running_task(&store, "task-b", "chat-turn-b").await;

        let won = claim(&store, "task-a", &attempt_a, 2200)
            .await
            .unwrap()
            .expect("first claim wins");
        let released = store
            .release_continuation_claim(&won.binding_id, &attempt_a)
            .await
            .unwrap()
            .expect("claimant releases its own reservation");
        assert_eq!(released.state, "active");
        assert!(released.claimed_attempt_id.is_none());
        assert!(released.claimed_task_id.is_none());
        assert!(released.claimed_at.is_none());
        assert_eq!(released.generation, won.generation + 1);
        // Releasing twice is an idempotent no-op.
        let again = store
            .release_continuation_claim(&won.binding_id, &attempt_a)
            .await
            .unwrap();
        assert_eq!(again.as_ref(), Some(&released));

        // The position is available again — and the other task wins it.
        let won_b = claim(&store, "task-b", &attempt_b, 2260)
            .await
            .unwrap()
            .expect("released position is claimable");
        assert_eq!(
            won_b.claimed_attempt_id.as_deref(),
            Some(attempt_b.as_str())
        );
        assert_eq!(won_b.generation, released.generation + 1);
        // The previous claimant cannot mark it uncertain any more: not
        // its row, never a takeover.
        assert!(
            store
                .mark_continuation_uncertain(&won.binding_id, &attempt_a)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn interrupted_claim_blocks_exact_resume_until_reconciled() {
        // Review round-4 F3-A reproduced: an attempt interrupted AFTER its
        // input was accepted leaves the vendor session having consumed the
        // prompt — the pre-claim position may never present as available
        // for an exact resume again.
        let store = store();
        let binding = published_position(&store).await;
        let attempt_a = running_task(&store, "task-a", "chat-turn-a").await;
        let won = claim(&store, "task-a", &attempt_a, 2200)
            .await
            .unwrap()
            .expect("claim wins");
        assert_eq!(won.state, "owned");

        // Interruption after input → uncertain; exact resume from the
        // pre-claim position is refused for EVERYONE, claimant included.
        let uncertain = store
            .mark_continuation_uncertain(&won.binding_id, &attempt_a)
            .await
            .unwrap()
            .expect("claimant marks its own claim uncertain");
        assert_eq!(uncertain.state, "uncertain");
        assert!(
            store
                .active_continuation(
                    "chat-1",
                    "msg-7",
                    7,
                    "claude-code",
                    "/usr/bin/claude",
                    "cfg-aaa",
                    Some("sp-1"),
                )
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .resumable_continuation(
                    "chat-1",
                    "msg-7",
                    7,
                    "claude-code",
                    "/usr/bin/claude",
                    "cfg-aaa",
                    Some("sp-1"),
                    &attempt_a,
                )
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            claim(&store, "task-a", &attempt_a, 2250)
                .await
                .unwrap()
                .is_none()
        );
        // Releasing is NOT a proof of non-acceptance once uncertain.
        assert!(
            store
                .release_continuation_claim(&won.binding_id, &attempt_a)
                .await
                .unwrap()
                .is_none()
        );

        // Settle the interrupted attempt, then a FRESH-session completion
        // advances the same vendor session: the commit sweep retires the
        // uncertain alias too (F3-A: retirement covers interrupted
        // attempts, not only successful completions).
        let lease_a = store
            .get_lease("task-a")
            .await
            .unwrap()
            .expect("interrupted lease");
        store
            .settle(SettleTask {
                fence: lease_a.fence(),
                now: 2300,
                status: TaskStatus::Failed,
                error: None,
                events: vec![],
                messages: vec![],
            })
            .await
            .unwrap();
        let attempt_b = running_task(&store, "task-b", "chat-turn-b").await;
        let mut advanced = commit("task-b", &attempt_b, "sess-1");
        advanced.parent = "msg-8".into();
        advanced.history_position = 8;
        advanced.committed_at = 2350;
        store
            .commit_continuation(advanced)
            .await
            .expect("advance commits under live authority");
        let prior = store
            .continuation_for_attempt(&binding.owning_attempt_id)
            .await
            .unwrap()
            .expect("prior row kept for audit");
        assert_eq!(
            prior.state, "superseded",
            "the uncertain alias must not survive the advance"
        );
    }

    #[tokio::test]
    async fn reconcile_returns_uncertain_position_to_service() {
        // The only edge that puts an uncertain row back: an explicit
        // reconcile proves the vendor position honest (e.g. the
        // interrupted turn's answer actually landed).
        let store = store();
        let _binding = published_position(&store).await;
        let attempt_a = running_task(&store, "task-a", "chat-turn-a").await;
        let won = claim(&store, "task-a", &attempt_a, 2200)
            .await
            .unwrap()
            .expect("claim wins");
        store
            .mark_continuation_uncertain(&won.binding_id, &attempt_a)
            .await
            .unwrap()
            .expect("uncertain");
        let reconciled = store
            .reconcile_continuation(&won.binding_id)
            .await
            .unwrap()
            .expect("uncertain → active");
        assert_eq!(reconciled.state, "active");
        assert!(reconciled.claimed_attempt_id.is_none());
        // …and it is claimable again by a fresh attempt.
        let attempt_b = running_task(&store, "task-b", "chat-turn-b").await;
        let won_b = claim(&store, "task-b", &attempt_b, 2300)
            .await
            .unwrap()
            .expect("reconciled position is available again");
        assert_eq!(
            won_b.claimed_attempt_id.as_deref(),
            Some(attempt_b.as_str())
        );
    }
    #[tokio::test]
    async fn lifecycle_start_survives_reopen_and_terminal_is_immutable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.db");
        let receipt;
        {
            let store = SqliteStore::open(&path, 4).unwrap();
            let attempt = running_task(&store, "effect-task", "effect-scope").await;
            receipt = NativeCallReceipt {
                attempt_id: attempt,
                task_id: "effect-task".into(),
                invocation_id: "effect-1".into(),
                tool_name: "write".into(),
                content_hash: "sha256:args".into(),
                record: serde_json::json!({"lifecycleState":"effectStarted", "outcome":{"contentHash":"sha256:args"}}),
                recorded_at: 1600,
            };
            store.record_call_lifecycle(receipt.clone()).await.unwrap();
        }
        let store = SqliteStore::open(&path, 4).unwrap();
        let reopened = store
            .get_call_receipt(&receipt.attempt_id, "effect-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            reopened.record["lifecycleState"], "effectStarted",
            "unfinished durable start proves possible execution after process loss"
        );
        let mut terminal = receipt.clone();
        terminal.record = serde_json::json!({"lifecycleState":"terminal","outcome":{"ok":true}});
        store.record_call_lifecycle(terminal.clone()).await.unwrap();
        assert!(
            store.record_call_lifecycle(receipt).await.is_err(),
            "settled invocation cannot restart"
        );
        let mut conflict = terminal.clone();
        conflict.record["outcome"] = serde_json::json!({"ok":false});
        assert!(store.record_call_lifecycle(conflict).await.is_err());
        let mut late = terminal.clone();
        late.record = serde_json::json!({"lifecycleState":"uncertain"});
        store.record_call_lifecycle(late).await.unwrap();
        assert_eq!(
            store
                .get_call_receipt(&terminal.attempt_id, "effect-1")
                .await
                .unwrap()
                .unwrap()
                .record,
            terminal.record
        );
    }

    #[tokio::test]
    async fn lifecycle_start_requires_live_lease() {
        let store = store();
        submit_task(&store, "not-running").await;
        assert!(
            store
                .record_call_lifecycle(NativeCallReceipt {
                    attempt_id: "stale".into(),
                    task_id: "not-running".into(),
                    invocation_id: "inv-1".into(),
                    tool_name: "write".into(),
                    content_hash: "hash".into(),
                    record: serde_json::json!({"lifecycleState":"effectStarted"}),
                    recorded_at: 1600,
                })
                .await
                .is_err()
        );
    }
}
