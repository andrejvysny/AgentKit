//! The SQLite adapter is the behavioural reference for the sessions port
//! (ADR 0024 §6): the suite `agentkit-testkit` runs against its in-memory
//! twin runs here too. The two durable states the port cannot create — a
//! parked turn and an admitted execution receipt — are seeded through the
//! store's own writers, the same split `task_repository_conformance` makes.

use agentkit_engine::{ClaimInput, LeaseFence, SettleTask, TaskStatus};
use agentkit_sessions::{AgentDefinition, CreateAgentInput, ExecutionReceiptRecord};
use agentkit_store::{
    AdmitTeammateTask, CreateAgent, ExecutionReceiptAdmission, ParkTurn, PinnedRunProfile,
    SqliteStore, StoreError, run_profile_digest,
};
use agentkit_testkit::conformance::session_repository::{self, SessionFixture};
use async_trait::async_trait;
use serde_json::{Value, json};

/// The suite's handle over the reference implementation.
#[derive(Clone)]
struct SqliteSessionFixture {
    store: SqliteStore,
}

impl SqliteSessionFixture {
    fn refused(message: impl Into<String>) -> StoreError {
        StoreError::InvalidInput(message.into())
    }

    /// The live lease of `task_id`. Claiming is the store's only writer that
    /// puts a turn in RUNNING, so the fixture drives it there rather than
    /// writing the row itself; a lease that already exists is reused as-is.
    async fn running(&self, task_id: &str, now: i64) -> Result<LeaseFence, StoreError> {
        if let Some(lease) = self.store.get_lease(task_id).await? {
            return Ok(lease.fence());
        }
        for _ in 0..16 {
            let Some(claimed) = self
                .store
                .claim_with_concurrency(
                    ClaimInput {
                        owner_id: "fixture".into(),
                        now,
                        ttl_ms: 600_000,
                        kinds: vec![],
                        busy_scopes: vec![],
                        max_attempts: 8,
                    },
                    usize::MAX,
                )
                .await?
            else {
                break;
            };
            if claimed.task.task_id == task_id {
                return Ok(claimed.lease.fence());
            }
            // Not the turn the case named: give the lease back so the claim
            // below can reach the one it wants.
            self.store.release_lease(&claimed.lease.lease_token).await?;
        }
        Err(Self::refused(format!(
            "fixture could not put turn {task_id} in play"
        )))
    }
}

#[async_trait]
impl SessionFixture for SqliteSessionFixture {
    type Repo = SqliteStore;
    type Error = StoreError;

    fn repo(&self) -> &SqliteStore {
        &self.store
    }

    async fn put_provider(&self, provider: Value, capabilities: Value) -> Result<(), StoreError> {
        let id = provider["id"]
            .as_str()
            .filter(|text| !text.is_empty())
            .ok_or_else(|| Self::refused("fixture provider needs an id"))?
            .to_owned();
        self.store.upsert_provider(provider).await?;
        self.store
            .save_provider_capabilities(&id, capabilities)
            .await
    }

    async fn delete_provider(&self, id: &str) -> Result<(), StoreError> {
        self.store.delete_provider(id).await
    }

    async fn put_provider_models(&self, id: &str, models: Vec<Value>) -> Result<(), StoreError> {
        self.store
            .replace_provider_models(id, models)
            .await
            .map(|_| ())
    }

    async fn put_skill(
        &self,
        source: &str,
        path: &str,
        content: &str,
    ) -> Result<Value, StoreError> {
        self.store
            .publish_skill(source.to_owned(), path.to_owned(), content.to_owned())
            .await
    }

    async fn put_setting(
        &self,
        namespace: &str,
        key: &str,
        value: Value,
    ) -> Result<(), StoreError> {
        self.store.set_setting(namespace, key, value).await
    }

    async fn put_agent(
        &self,
        agent_id: &str,
        definition: AgentDefinition,
        now: i64,
    ) -> Result<(), StoreError> {
        self.store
            .create_agent(CreateAgent {
                agent_id: Some(agent_id.to_owned()),
                scope: format!("agent:{agent_id}"),
                owner_id: "fixture".into(),
                definition,
                now,
            })
            .await
            .map(|_| ())
    }

    async fn park_turn(
        &self,
        task_id: &str,
        request: CreateAgentInput,
        now: i64,
    ) -> Result<String, StoreError> {
        let fence = self.running(task_id, now).await?;
        self.store
            .park_turn(ParkTurn {
                fence,
                assistant_message: None,
                events: vec![],
                request: Some(request),
                now,
            })
            .await?
            .ok_or_else(|| Self::refused(format!("fixture could not park turn {task_id}")))
    }

    async fn put_events(
        &self,
        task_id: &str,
        events: Vec<Value>,
        now: i64,
    ) -> Result<(), StoreError> {
        let fence = self.running(task_id, now).await?;
        self.store
            .append_events(fence, events, now)
            .await
            .map(|_| ())
    }

    async fn admit_receipt(
        &self,
        receipt: ExecutionReceiptRecord,
        settle_run: bool,
        now: i64,
    ) -> Result<(), StoreError> {
        let fence = self.running(&receipt.run_id, now).await?;
        let admitted = self
            .store
            .admit_execution(
                fence.clone(),
                ExecutionReceiptAdmission {
                    execution_id: receipt.execution_id.clone(),
                    run_id: receipt.run_id.clone(),
                    chat_id: receipt.chat_id.clone(),
                    tool_call_id: receipt.tool_call_id.clone(),
                    tool_name: receipt.tool_name.clone(),
                    args_sha256: receipt.args_sha256.clone(),
                    ts_args_sha256: None,
                    args_hash_provenance: receipt.args_hash_provenance,
                    context_sha256: receipt.context_sha256.clone(),
                    attempt_id: receipt.attempt_id.clone(),
                    approval_request_id: receipt.approval_request_id.clone(),
                    approval_generation: receipt.approval_generation.clone(),
                    now,
                },
            )
            .await?;
        if !admitted {
            return Err(Self::refused(format!(
                "fixture admission of {} was refused",
                receipt.execution_id
            )));
        }
        if settle_run {
            // The run ends as a crash would: no result is recorded, so the
            // admitted receipt stays the uncertain one the resolver answers.
            self.store
                .settle(SettleTask {
                    fence,
                    now,
                    status: TaskStatus::Failed,
                    error: Some("fixture crash".into()),
                    events: vec![],
                    messages: vec![],
                })
                .await?;
        }
        Ok(())
    }

    async fn put_pinned_turn(
        &self,
        task_id: &str,
        agent_id: &str,
        profile: Value,
        now: i64,
    ) -> Result<(), StoreError> {
        if self.store.get_task(task_id).await?.is_some() {
            return Ok(());
        }
        self.store
            .admit_teammate_task(AdmitTeammateTask {
                task_id: Some(task_id.to_owned()),
                agent_id: agent_id.to_owned(),
                admission_key: format!("fixture-{task_id}"),
                kind: "teammate.turn".into(),
                payload: json!({"key": task_id}),
                priority: 0,
                now,
                available_at: None,
                depends_on: vec![],
                parent_task_id: None,
                run_profile: Some(PinnedRunProfile {
                    digest: run_profile_digest(&profile),
                    profile,
                    pinned_at: now,
                }),
            })
            .await
            .map(|_| ())
    }
}

#[tokio::test]
async fn sqlite_conforms() {
    session_repository::run(|| async {
        SqliteSessionFixture {
            store: SqliteStore::memory(16).unwrap(),
        }
    })
    .await;
}
