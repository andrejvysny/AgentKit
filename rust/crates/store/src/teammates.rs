//! Durable teammate admission, mailbox ordering, and run ownership
//! (AGENTKIT-88) on the existing [`SqliteStore`] task engine.
//!
//! A *teammate run* is a logical task admitted to execute under a
//! persistent agent identity (AGENTKIT-87). This module adds exactly the
//! durability the contract names — nothing else:
//!
//! - **Transactional idempotent admission.** [`SqliteStore::admit_teammate_task`]
//!   resolves the agent (unknown/deleted is absence, archived refuses new
//!   admission — the same gate as
//!   [`SqliteStore::check_agent_admission`], enforced INSIDE the
//!   admission transaction so archive and admit serialize on one row of
//!   truth), pins the agent's current definition revision as the task's
//!   immutable [`TaskRecord::agent_revision`], and records the caller's
//!   idempotency key in the `agent_admissions` ledger — all in ONE
//!   transaction. Two competing admits under one key produce exactly one
//!   logical task; the loser replays the winner's record
//!   ([`AdmittedTask::replayed`]). Reconnecting clients and history loads
//!   read tasks; they never create them.
//! - **One active state-mutating task per agent, bounded shared pool.**
//!   Admission queues freely; `claim` serializes execution — an agent's
//!   lane admits a single running attempt at a time (across scopes),
//!   while the pool's `concurrency` bound is shared across agents.
//!   Priority order, cancellation, and the [`TaskAging`] starvation valve
//!   are the existing scheduler primitives, unchanged.
//! - **Durable ordered mailbox.** Pending inputs addressed to an agent
//!   live in the existing `agent_inputs` queue under its `agent_id`
//!   column (no second queue): idempotent on `input_id`, store-assigned
//!   FIFO `order`, promotion strictly oldest-first.
//! - **Waiting releases capacity without losing ownership.**
//!   [`SqliteStore::wait_for_children`] parks a running attempt on
//!   unfinished children exactly like the approval park: the attempt
//!   ends, the lease is released (so the slot returns to the pool and
//!   nothing spins), and the task keeps owning its agent lane until the
//!   wait resolves through the queue.
//! - **Fenced late results and uncertain outcomes.** Every park/settle
//!   path stays behind the lease fence, so a stale generation is refused
//!   outright. When an attempt's outcome cannot be decided (crash, lease
//!   expiry mid-run), [`SqliteStore::reconcile_agent_runs`] moves the
//!   orphan to `Uncertain` — never silently re-running a state-mutating
//!   attempt — and only an explicit [`SqliteStore::resolve_uncertain`]
//!   settles it. Admission reconciles first: unfinished runs are
//!   classified before new work is admitted.
//!
//! Agent DISPLAY state is a pure derived projection
//! ([`derive_agent_display_state`]) over the task rows — there is no
//! mutable state column that could drift from the tasks it describes.

use crate::{
    AgentInputKind, AgentInputRecord, AgentInputStatus, AgentLifecycle, Result, SqliteStore,
    StoreError, TaskBinding, TaskRecord, TaskStatus,
    agent_inputs::{get_input, next_order, transition_input},
    util,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The scope every admitted task lives in. The agent LANE (the
/// one-active-writer guarantee) is enforced on the bound `agent_id`, not
/// on this string, so tasks in a deliberately distinct scope still share
/// one lane while scope-based housekeeping stays conventional.
pub fn teammate_scope(agent_id: &str) -> String {
    format!("agent:{agent_id}")
}

/// The admission payload. `task_id` may be pinned for a caller-owned
/// logical id (an idempotent retry from a client that knows it); `None`
/// mints one. `admission_key` is the idempotency key: the one value that
/// decides whether this call creates a logical task or replays the one
/// the same key already created.
#[derive(Clone, Debug)]
pub struct AdmitTeammateTask {
    pub task_id: Option<String>,
    pub agent_id: String,
    pub admission_key: String,
    pub kind: String,
    pub payload: Value,
    pub priority: i32,
    pub now: i64,
    pub available_at: Option<i64>,
    pub depends_on: Vec<String>,
    pub parent_task_id: Option<String>,
    /// AGENTKIT-89: the ONE immutable effective run profile, resolved and
    /// sealed by the host before admission. It rides the admission
    /// transaction as part of the task's durable binding — validation of
    /// everything it references (provider, capabilities, skill revisions,
    /// sources, approvals) runs INSIDE that transaction, so an admission
    /// either pins a profile that was live at its commit instant or
    /// commits nothing. `None` is a pre-89 admission (no profile).
    pub run_profile: Option<crate::PinnedRunProfile>,
}

/// The admission receipt. `replayed` distinguishes the call that created
/// the logical task from a duplicate key returning the same one — both
/// return the same record, only one of them wrote it.
#[derive(Clone, Debug, PartialEq)]
pub struct AdmittedTask {
    pub task: TaskRecord,
    pub replayed: bool,
}

/// The `agent_admissions` ledger row (the key itself is the lookup):
/// the durable proof that `admission_key` created exactly `task_id`
/// under `agent_id`.
#[derive(Clone, Debug, PartialEq)]
struct AdmissionRow {
    agent_id: String,
    task_id: String,
}

fn read_admission(db: &Connection, admission_key: &str) -> Result<Option<AdmissionRow>> {
    db.query_row(
        "SELECT agent_id,task_id FROM agent_admissions WHERE admission_key=?",
        [admission_key],
        |row| {
            Ok(AdmissionRow {
                agent_id: row.get(0)?,
                task_id: row.get(1)?,
            })
        },
    )
    .optional()
    .map_err(Into::into)
}

/// The derived agent display state — the answer to "what should the UI
/// say about this agent right now". It is computed from the agent's
/// task rows on every read; nothing stores it, so it cannot drift.
/// Precedence is the operator's need to act: an in-flight run (running
/// or parked on children) reads `Working`; a parked human request reads
/// `NeedsYou`; an outcome that cannot be decided reads `Error`; an
/// interrupted run reads `Interrupted`; queued work reads `Queued`;
/// otherwise the agent is `Idle`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentDisplayState {
    Idle,
    Queued,
    Working,
    NeedsYou,
    Interrupted,
    Error,
}

/// The one place display state is computed. Pure: same tasks in, same
/// state out, no clock, no store.
pub fn derive_agent_display_state(tasks: &[TaskRecord]) -> AgentDisplayState {
    let live = |statuses: &[TaskStatus]| {
        tasks
            .iter()
            .any(|task| statuses.contains(&task.status) && task.dead_lettered_at.is_none())
    };
    if live(&[TaskStatus::Running, TaskStatus::WaitingChild]) {
        AgentDisplayState::Working
    } else if live(&[TaskStatus::WaitingApproval]) {
        AgentDisplayState::NeedsYou
    } else if live(&[TaskStatus::Uncertain]) {
        AgentDisplayState::Error
    } else if live(&[TaskStatus::Interrupted]) {
        AgentDisplayState::Interrupted
    } else if live(&[TaskStatus::Queued]) {
        AgentDisplayState::Queued
    } else {
        AgentDisplayState::Idle
    }
}

/// The admission gate, evaluated inside the admission transaction.
/// Mirrors [`SqliteStore::check_agent_admission`]: a tombstone or unknown
/// id is absence; archived refuses NEW admission. Reading the row on the
/// caller's open transaction is what makes archive-during-admission
/// serialize instead of race.
fn admission_gate(db: &Connection, agent_id: &str) -> Result<crate::AgentRecord> {
    let record: Option<crate::AgentRecord> = util::get(db, "agents", "agent_id", agent_id)?;
    let Some(record) = record else {
        return Err(StoreError::NotFound(format!("agent {agent_id}")));
    };
    match record.lifecycle {
        AgentLifecycle::Active => Ok(record),
        AgentLifecycle::Deleted => Err(StoreError::NotFound(format!("agent {agent_id}"))),
        AgentLifecycle::Archived => Err(StoreError::InvalidTransition(format!(
            "agent_archived: {agent_id} refuses new admission"
        ))),
    }
}

fn unfinished_children(db: &Connection, task_id: &str) -> Result<bool> {
    db.query_row(
        "SELECT EXISTS(SELECT 1 FROM tasks \
         WHERE json_extract(record,'$.parentTaskId')=? \
         AND status NOT IN ('completed','failed','cancelled'))",
        [task_id],
        |row| row.get(0),
    )
    .map_err(Into::into)
}

/// End every still-`running` attempt row of a task (the lease-less
/// recovery shape: `lifecycle::end_attempt` needs a live lease; a
/// reconciled orphan by definition has none). FIRST TERMINAL WINS per
/// attempt, exactly like every other attempt end.
fn abandon_running_attempts(db: &Connection, task_id: &str, now: i64, error: &str) -> Result<()> {
    let rows: Vec<(String, String)> = db
        .prepare(
            "SELECT id, record FROM attempts \
             WHERE task_id=? AND json_extract(record,'$.status')='running'",
        )?
        .query_map([task_id], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (id, text) in rows {
        let mut attempt: crate::AttemptRecord = serde_json::from_str(&text)?;
        attempt.status = "abandoned".into();
        attempt.ended_at = Some(now);
        attempt.error = Some(error.into());
        db.execute(
            "UPDATE attempts SET record=? WHERE id=?",
            params![util::json(db, &attempt)?, id],
        )?;
    }
    Ok(())
}

impl SqliteStore {
    /// The single admission path: agent gate + logical task insert +
    /// immutable revision pin + idempotency ledger, ONE transaction.
    ///
    /// Duplicate `admission_key` returns the task that key already
    /// created (`replayed == true`) after verifying the replay carries
    /// the same identity, kind, payload, and parent — a key is reserved
    /// for ONE logical task, never re-pointed at different work.
    pub async fn admit_teammate_task(&self, input: AdmitTeammateTask) -> Result<AdmittedTask> {
        if input.agent_id.is_empty() || input.admission_key.is_empty() || input.kind.is_empty() {
            return Err(StoreError::InvalidInput(
                "agent id, admission key, and kind must be nonempty".into(),
            ));
        }
        if input.task_id.as_deref().is_some_and(str::is_empty) {
            return Err(StoreError::InvalidInput(
                "pinned task id must be nonempty".into(),
            ));
        }
        self.call(move |db| {
            let transaction = crate::transaction::atomic(db)?;
            // Idempotent replay: the ledger names the task this key
            // created; identity is verified against the durable record,
            // never against the caller's claim.
            let existing = read_admission(&transaction, &input.admission_key)?;
            if let Some(row) = existing {
                let task: TaskRecord =
                    util::required(&transaction, "tasks", "id", &row.task_id)?;
                if row.agent_id != input.agent_id
                    || task.agent_id.as_deref() != Some(input.agent_id.as_str())
                    || task.kind != input.kind
                    || task.payload != input.payload
                    || task.parent_task_id != input.parent_task_id
                    || task.depends_on != input.depends_on
                    // AGENTKIT-89: one key admitted ONE profile; a replay
                    // claiming a different seal is different work.
                    || task.run_profile.as_ref().map(|pin| &pin.digest)
                        != input.run_profile.as_ref().map(|pin| &pin.digest)
                    || input
                        .task_id
                        .as_deref()
                        .is_some_and(|id| id != task.task_id)
                {
                    return Err(StoreError::InvalidInput(format!(
                        "admission replay identity mismatch: key {} already admitted different work",
                        input.admission_key
                    )));
                }
                return Ok(AdmittedTask {
                    task,
                    replayed: true,
                });
            }
            // The gate runs on THIS transaction: archiving the agent
            // serializes against admitting it — one of the two wins,
            // and an admit that loses answers `agent_archived` having
            // committed nothing.
            let agent = admission_gate(&transaction, &input.agent_id)?;
            // AGENTKIT-89: the effective-profile seal and every durable
            // fact it references verify HERE, on the admission
            // transaction — the pin becomes part of the admitted task's
            // durable binding in the same commit, never a second ledger.
            crate::run_profiles::validate_at_admission(
                &transaction,
                &agent,
                input.run_profile.as_ref(),
            )?;
            // A pinned task id that already exists is a replay only when
            // it is the very same admission; otherwise it is a collision
            // the caller must not paper over.
            if let Some(pinned) = input.task_id.as_deref()
                && let Some(task) =
                    util::get::<TaskRecord>(&transaction, "tasks", "id", pinned)?
            {
                if task.agent_id.as_deref() == Some(input.agent_id.as_str())
                    && task.admission_key.as_deref() == Some(input.admission_key.as_str())
                {
                    // Ledger row missing for an admitted task is
                    // impossible by construction (one transaction);
                    // the task itself is the replay either way.
                    return Ok(AdmittedTask {
                        task,
                        replayed: true,
                    });
                }
                return Err(StoreError::Duplicate(pinned.to_owned()));
            }
            let task_id = input.task_id.clone().unwrap_or_else(util::id);
            let task = crate::tasks::insert_bound(
                &transaction,
                crate::SubmitTask {
                    task_id: task_id.clone(),
                    kind: input.kind,
                    scope_id: teammate_scope(&input.agent_id),
                    payload: input.payload,
                    priority: input.priority,
                    now: input.now,
                    available_at: input.available_at,
                    depends_on: input.depends_on,
                    parent_task_id: input.parent_task_id,
                    exclusive_scope: false,
                },
                Some(TaskBinding {
                    agent_id: input.agent_id.clone(),
                    agent_revision: agent.current_revision,
                    admission_key: input.admission_key.clone(),
                    run_profile: input.run_profile.clone(),
                }),
            )?;
            transaction.execute(
                "INSERT INTO agent_admissions VALUES(?,?,?,?)",
                params![input.admission_key, input.agent_id, task_id, input.now],
            )?;
            crate::tasks::lifecycle_event(
                &transaction,
                &task,
                "task.admitted",
                input.now,
            )?;
            transaction.commit()?;
            Ok(AdmittedTask {
                task,
                replayed: false,
            })
        })
        .await
    }

    /// The admission ledger read: the logical task an idempotency key
    /// created, if any. Read-only — history load never creates.
    pub async fn task_by_admission_key(&self, admission_key: String) -> Result<Option<TaskRecord>> {
        self.call(move |db| {
            let Some(row) = read_admission(db, &admission_key)? else {
                return Ok(None);
            };
            util::get(db, "tasks", "id", &row.task_id)
        })
        .await
    }

    /// Every task bound to one agent, insertion-ordered. The source rows
    /// of the derived display state and of run-history load.
    pub async fn list_agent_tasks(&self, agent_id: String) -> Result<Vec<TaskRecord>> {
        self.call(move |db| {
            util::select(
                db,
                "SELECT record FROM tasks WHERE json_extract(record,'$.agentId')=? ORDER BY rowid",
                [&agent_id],
            )
        })
        .await
    }

    /// Park a running attempt on its unfinished children, mirroring the
    /// approval park exactly: one transaction moves `Running` to
    /// `WaitingChild`, ends the attempt normally, and releases the lease
    /// — so the attempt stops counting against the bounded pool (waiting
    /// never spins or reserves a slot) while the task keeps owning its
    /// agent lane. Returns `false` (committing nothing but the fence
    /// check) when no children are unfinished, so the caller proceeds to
    /// settle; a stale fence loses with [`StoreError::LeaseLost`].
    pub async fn wait_for_children(&self, proof: crate::LeaseFence, now: i64) -> Result<bool> {
        self.call(move |db| {
            let transaction = crate::transaction::atomic(db)?;
            let lease = crate::tasks::fence(&transaction, &proof, now)?;
            let mut task: TaskRecord = util::required(&transaction, "tasks", "id", &proof.task_id)?;
            if task.status != TaskStatus::Running || task.cancel_requested {
                return Err(StoreError::LeaseLost(proof.task_id));
            }
            if !unfinished_children(&transaction, &task.task_id)? {
                transaction.commit()?;
                return Ok(false);
            }
            task.status = TaskStatus::WaitingChild;
            crate::tasks::save(&transaction, &task)?;
            crate::tasks::lifecycle_event(&transaction, &task, "task.waiting_child", now)?;
            crate::tasks::end_attempt(&transaction, &lease, "completed", now, None)?;
            transaction.execute("DELETE FROM leases WHERE task_id=?", [&task.task_id])?;
            transaction.commit()?;
            Ok(true)
        })
        .await
    }

    /// Resolve a child wait: `WaitingChild` goes back to `Queued` so the
    /// next claim mints a FRESH attempt (attempts are minted by `claim`,
    /// never by a status write). Only a parked wait may take this edge.
    pub async fn continue_after_children(&self, task_id: String, now: i64) -> Result<TaskRecord> {
        self.call(move |db| {
            let transaction = crate::transaction::atomic(db)?;
            let mut task: TaskRecord = util::required(&transaction, "tasks", "id", &task_id)?;
            if task.status != TaskStatus::WaitingChild {
                return Err(StoreError::InvalidTransition(format!(
                    "{:?} cannot continue from a child wait",
                    task.status
                )));
            }
            task.status = TaskStatus::Queued;
            task.available_at = now;
            crate::tasks::save(&transaction, &task)?;
            crate::tasks::lifecycle_event(&transaction, &task, "task.continued", now)?;
            transaction.commit()?;
            Ok(task)
        })
        .await
    }

    /// Record that a running attempt's outcome cannot be decided (the
    /// execution path lost its ability to attest): fenced, ends the
    /// attempt `abandoned`, releases the lease, and parks the task in
    /// `Uncertain` — lane-owning, unclaimable, terminal only by explicit
    /// resolution. This is the state a late worker must never be able to
    /// overwrite with a stale result: every write path it would use
    /// requires the lease this just dropped.
    pub async fn mark_uncertain(
        &self,
        proof: crate::LeaseFence,
        now: i64,
        reason: &str,
    ) -> Result<TaskRecord> {
        let reason = reason.to_owned();
        self.call(move |db| {
            let transaction = crate::transaction::atomic(db)?;
            let lease = crate::tasks::fence(&transaction, &proof, now)?;
            let mut task: TaskRecord = util::required(&transaction, "tasks", "id", &proof.task_id)?;
            if task.status != TaskStatus::Running {
                return Err(StoreError::InvalidTransition(format!(
                    "{:?} cannot become uncertain",
                    task.status
                )));
            }
            task.status = TaskStatus::Uncertain;
            task.error = Some(reason.clone());
            crate::tasks::save(&transaction, &task)?;
            crate::tasks::end_attempt(&transaction, &lease, "abandoned", now, Some(reason))?;
            crate::tasks::lifecycle_event(&transaction, &task, "task.uncertain", now)?;
            transaction.execute("DELETE FROM leases WHERE task_id=?", [&task.task_id])?;
            transaction.commit()?;
            Ok(task)
        })
        .await
    }

    /// Settle an `Uncertain` outcome explicitly: a terminal status, or
    /// back to `Queued` for a NEW attempt on the same logical task
    /// (retries are attempts, never new tasks). Anything else is refused
    /// — uncertainty is resolved by evidence, not by waiting it out.
    pub async fn resolve_uncertain(
        &self,
        task_id: String,
        to: TaskStatus,
        error: Option<String>,
        now: i64,
    ) -> Result<TaskRecord> {
        self.call(move |db| {
            if !matches!(
                to,
                TaskStatus::Completed
                    | TaskStatus::Failed
                    | TaskStatus::Cancelled
                    | TaskStatus::Queued
            ) {
                return Err(StoreError::InvalidInput(
                    "uncertain outcomes resolve only to completed, failed, cancelled, or queued"
                        .into(),
                ));
            }
            let transaction = crate::transaction::atomic(db)?;
            let mut task: TaskRecord = util::required(&transaction, "tasks", "id", &task_id)?;
            if task.status != TaskStatus::Uncertain {
                return Err(StoreError::InvalidTransition(format!(
                    "{:?} is not uncertain",
                    task.status
                )));
            }
            if to.is_terminal() {
                task.status = to;
                task.finished_at = Some(now);
                task.error = error;
                crate::tasks::save(&transaction, &task)?;
                crate::tasks::lifecycle_event(&transaction, &task, "task.settled", now)?;
            } else {
                task.status = TaskStatus::Queued;
                task.available_at = now;
                task.retry_pending = false;
                crate::tasks::save(&transaction, &task)?;
                crate::tasks::lifecycle_event(&transaction, &task, "task.queued", now)?;
            }
            transaction.commit()?;
            Ok(task)
        })
        .await
    }

    /// Reconcile an agent's unfinished runs BEFORE admission resumes.
    /// Called by the host admission path (and at startup) so a crashed
    /// attempt is classified, not guessed:
    /// - `Running` with no lease, or a lease already expired at `now`, is
    ///   an orphan: its attempts end `abandoned`, the lease residue
    ///   clears, and the task parks in `Uncertain` — it is NOT requeued
    ///   automatically, because a state-mutating attempt whose result is
    ///   unknown must not run twice. An orphan whose durable
    ///   `cancel_requested` flag was written is instead landed
    ///   `Cancelled`: that request is evidence, so its outcome IS
    ///   decidable.
    /// - `Running` under a live lease belongs to a worker that is still
    ///   attested; reconciliation leaves it alone.
    /// - `Queued` rows with a durable cancel land `Cancelled` now
    ///   (nothing is running to notice).
    /// - `WaitingApproval` / `WaitingChild` / `Interrupted` / `Uncertain`
    ///   rows already own their wait and stay untouched.
    ///
    /// Returns the tasks this pass moved.
    pub async fn reconcile_agent_runs(
        &self,
        agent_id: String,
        now: i64,
    ) -> Result<Vec<TaskRecord>> {
        self.call(move |db| {
            let transaction = crate::transaction::atomic(db)?;
            let tasks: Vec<TaskRecord> = util::select(
                &transaction,
                "SELECT record FROM tasks WHERE json_extract(record,'$.agentId')=? ORDER BY rowid",
                [&agent_id],
            )?;
            let mut changed = Vec::new();
            for mut task in tasks {
                match task.status {
                    TaskStatus::Running => {
                        let lease: Option<crate::Lease> =
                            util::get(&transaction, "leases", "task_id", &task.task_id)?;
                        if lease.as_ref().is_some_and(|lease| lease.expires_at > now) {
                            continue;
                        }
                        abandon_running_attempts(
                            &transaction,
                            &task.task_id,
                            now,
                            "lease_expired_unresolved",
                        )?;
                        if lease.is_some() {
                            transaction
                                .execute("DELETE FROM leases WHERE task_id=?", [&task.task_id])?;
                        }
                        if task.cancel_requested {
                            // The durable cancel was written but its
                            // worker died before observing it: honor the
                            // request — this outcome IS decidable.
                            task.status = TaskStatus::Cancelled;
                            task.finished_at = Some(now);
                            task.retry_pending = false;
                            task.interrupt_requested = false;
                            crate::tasks::save(&transaction, &task)?;
                            crate::tasks::lifecycle_event(
                                &transaction,
                                &task,
                                "task.cancel_requested",
                                now,
                            )?;
                        } else {
                            task.status = TaskStatus::Uncertain;
                            task.error = Some("lease_expired_unresolved".into());
                            crate::tasks::save(&transaction, &task)?;
                            crate::tasks::lifecycle_event(
                                &transaction,
                                &task,
                                "task.uncertain",
                                now,
                            )?;
                        }
                        changed.push(task);
                    }
                    TaskStatus::Queued if task.cancel_requested => {
                        task.status = TaskStatus::Cancelled;
                        task.finished_at = Some(now);
                        task.retry_pending = false;
                        task.interrupt_requested = false;
                        crate::tasks::save(&transaction, &task)?;
                        crate::tasks::lifecycle_event(
                            &transaction,
                            &task,
                            "task.cancel_requested",
                            now,
                        )?;
                        changed.push(task);
                    }
                    _ => {}
                }
            }
            transaction.commit()?;
            Ok(changed)
        })
        .await
    }

    /// Enqueue a durable pending input for an agent — the mailbox write.
    /// Idempotent on `input_id` (same agent, kind, and content replays
    /// the existing row; different facts under one id conflict), ordered
    /// by the store-assigned strictly-increasing `order`. Mailbox inputs
    /// are steers and follow-ups: a human request belongs to an attempt,
    /// not to a mailbox, so it is refused here.
    pub async fn enqueue_agent_mailbox(
        &self,
        agent_id: String,
        input: crate::CreateAgentMailbox,
    ) -> Result<(AgentInputRecord, bool)> {
        self.call(move |db| {
            let transaction = crate::transaction::atomic(db)?;
            let record = insert_mailbox(&transaction, &agent_id, input)?;
            transaction.commit()?;
            Ok(record)
        })
        .await
    }

    /// The mailbox read: pending (or otherwise) inputs of one agent in
    /// strict FIFO `order`. The ordering key is store-assigned, so this
    /// is the durable queue order, not a caller hint.
    pub async fn list_agent_mailbox(
        &self,
        agent_id: String,
        statuses: Option<Vec<AgentInputStatus>>,
    ) -> Result<Vec<AgentInputRecord>> {
        self.call(move |db| {
            let mut statement = db.prepare(
                "SELECT record FROM agent_inputs WHERE agent_id=? \
                 ORDER BY CAST(json_extract(record,'$.order') AS INTEGER)",
            )?;
            let rows = statement.query_map([&agent_id], |row| row.get::<_, String>(0))?;
            let mut records = Vec::new();
            for row in rows {
                let text = row?;
                let value = agentkit_contracts::parse_javascript_json(&text)
                    .map_err(|error| StoreError::RecoveryUnreadable(error.to_string()))?;
                let record: AgentInputRecord = serde_json::from_value(value)?;
                if statuses
                    .as_ref()
                    .is_some_and(|statuses| !statuses.contains(&record.status))
                {
                    continue;
                }
                records.push(record);
            }
            Ok(records)
        })
        .await
    }

    /// Promote the OLDEST pending mailbox input of one agent, compare-
    /// and-set against the per-kind machine (`pending → converted`). Two
    /// promoters never take the same input twice — the loser sees the
    /// CAS refuse — and out-of-order promotion is impossible because
    /// this is the only promotion path and it always takes the head of
    /// the durable order.
    pub async fn promote_next_agent_mailbox(
        &self,
        agent_id: String,
        now: i64,
    ) -> Result<Option<AgentInputRecord>> {
        self.call(move |db| {
            let transaction = crate::transaction::atomic(db)?;
            let head: Option<AgentInputRecord> = get(
                &transaction,
                "SELECT record FROM agent_inputs \
                 WHERE agent_id=? AND status='pending' \
                 ORDER BY CAST(json_extract(record,'$.order') AS INTEGER) LIMIT 1",
                &agent_id,
            )?;
            let Some(head) = head else {
                transaction.commit()?;
                return Ok(None);
            };
            let now_text = crate::ConversationTimestamp::from_epoch_millis(now)?
                .wire_text()?
                .into_owned();
            let record = transition_input(
                &transaction,
                &head.input_id,
                &[AgentInputStatus::Pending],
                AgentInputStatus::Converted,
                &crate::AgentInputPatch::default(),
                &now_text,
            )?;
            transaction.commit()?;
            Ok(Some(record))
        })
        .await
    }

    /// Cancel a still-pending mailbox input by id. Compare-and-set: an
    /// already promoted or cancelled input answers with the machine's
    /// refusal instead of pretending.
    pub async fn cancel_agent_mailbox(
        &self,
        input_id: String,
        now: i64,
    ) -> Result<AgentInputRecord> {
        self.call(move |db| {
            let transaction = crate::transaction::atomic(db)?;
            let now_text = crate::ConversationTimestamp::from_epoch_millis(now)?
                .wire_text()?
                .into_owned();
            let record = transition_input(
                &transaction,
                &input_id,
                &[AgentInputStatus::Pending],
                AgentInputStatus::Cancelled,
                &crate::AgentInputPatch::default(),
                &now_text,
            )?;
            transaction.commit()?;
            Ok(record)
        })
        .await
    }
}

/// Shared row reader for single-record `SELECT record`-shaped queries on
/// tables whose primary key is not a plain column lookup.
fn get<T: serde::de::DeserializeOwned>(db: &Connection, sql: &str, key: &str) -> Result<Option<T>> {
    db.query_row(sql, [key], |row| row.get::<_, String>(0))
        .optional()?
        .map(|text| serde_json::from_str(&text).map_err(Into::into))
        .transpose()
}

/// The mailbox insert: same queue, same counters, same record shape as
/// the session inputs — addressed to an agent through `agent_id`.
fn insert_mailbox(
    db: &Connection,
    agent_id: &str,
    input: crate::CreateAgentMailbox,
) -> Result<(AgentInputRecord, bool)> {
    if agent_id.is_empty() || input.input_id.is_empty() || input.content_hash.is_empty() {
        return Err(StoreError::InvalidInput(
            "mailbox agent id, input id, and content hash must be nonempty".into(),
        ));
    }
    if !matches!(input.kind, AgentInputKind::Steer | AgentInputKind::FollowUp) {
        return Err(StoreError::InvalidInput(
            "mailbox inputs are steers or follow-ups; human requests belong to an attempt".into(),
        ));
    }
    if input
        .expires_at
        .as_deref()
        .is_some_and(|value| crate::agent_inputs::parse_iso_millis(value).is_none())
    {
        return Err(StoreError::InvalidInput(
            "expiresAt must be a valid ISO timestamp.".into(),
        ));
    }
    if let Some(existing) = get_input(db, &input.input_id)? {
        if existing.agent_id.as_deref() != Some(agent_id)
            || existing.kind != input.kind
            || existing.content_hash != input.content_hash
        {
            return Err(StoreError::Duplicate(format!(
                "input_conflict: input {} already exists with different agent, kind, or content.",
                input.input_id
            )));
        }
        return Ok((existing, false));
    }
    let now_text = crate::ConversationTimestamp::from_epoch_millis(input.now)?
        .wire_text()?
        .into_owned();
    let record = AgentInputRecord {
        input_id: input.input_id,
        // The mailbox is addressed by `agent_id`; the session column
        // mirrors it so queue-wide invariants (one owner per row, NOT
        // NULL) hold honestly — this row belongs to the agent, and
        // session read paths filter on `agent_id IS NULL`.
        session_id: agent_id.to_owned(),
        agent_id: Some(agent_id.to_owned()),
        kind: input.kind,
        status: AgentInputStatus::Pending,
        content: input.content,
        content_hash: input.content_hash,
        order: next_order(db)?,
        target_task_id: None,
        on_settled: None,
        attempt_id: None,
        tool_call_id: None,
        expires_at: input.expires_at,
        applied_task_id: None,
        applied_message_id: None,
        answer_id: None,
        created_at: now_text.clone(),
        updated_at: now_text,
    };
    db.execute(
        "INSERT INTO agent_inputs VALUES(?,?,?,?,?,?,?)",
        params![
            record.input_id,
            record.session_id,
            crate::agent_inputs::kind_text(record.kind),
            crate::agent_inputs::status_text(record.status),
            record.target_task_id,
            util::json(db, &record)?,
            record.agent_id,
        ],
    )?;
    Ok((record, true))
}

/// The mailbox create payload. `input_id` is the caller's idempotency
/// key for the input (distinct from the admission key, which keys a
/// logical task); `content_hash` is what tells a replay from a conflict.
#[derive(Clone, Debug)]
pub struct CreateAgentMailbox {
    pub input_id: String,
    pub kind: AgentInputKind,
    pub content: Value,
    pub content_hash: String,
    pub expires_at: Option<String>,
    /// The caller's clock for the record timestamps.
    pub now: i64,
}
