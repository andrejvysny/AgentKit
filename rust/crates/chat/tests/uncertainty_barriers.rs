//! Durable uncertainty barriers through governance staging and the real
//! ledger (ADR 0020, ADR 0038): every barrier that matches a call counts.
//! Two of them end the call with the uncertain refusal naming both (one
//! answer names one generation, so no answer could open the call); an
//! operator resolves one by id, the call then asks about the other, and
//! one yes runs it exactly once.

use agentkit_core::{
    CancellationToken, CoreError, SuspendSlot, Tool, ToolContext, ToolDefinition, ToolLimits,
    ToolResult,
};
use agentkit_governance::{
    ApprovalGrant, ContributedTool, DurableExecution, DurableExecutionPort, GovernanceResult,
    ToolContributionContext, ToolContributor, args_sha256, stage_tools,
};
use agentkit_store::{
    AgentInputKind, AgentInputPatch, AgentInputStatus, ArgsHashProvenance, ClaimInput,
    CreateAgentInput, SqliteStore, SubmitTask, UncertaintyBarrierRecord, barrier_id_of,
};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

const CHAT: &str = "chat";
const RUN: &str = "run";

type Context = agentkit_engine::ExecutionContext<SqliteStore>;

fn now() -> i64 {
    agentkit_engine::Runtime::now_ms(&agentkit_engine::SystemRuntime)
}

fn input() -> Value {
    json!({"path": "a.txt"})
}

struct Mutate {
    definition: ToolDefinition,
    runs: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for Mutate {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }
    async fn execute(&self, context: ToolContext, _: Value) -> Result<ToolResult, CoreError> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult {
            ok: true,
            data: Some(json!({"done": true})),
            sources: vec![],
            warnings: vec![],
            truncated: false,
            limits: context.limits,
            model_data: None,
            summary: None,
            status: None,
            model_content: None,
        })
    }
}

struct Contributor(Arc<Mutate>);

#[async_trait]
impl ToolContributor for Contributor {
    fn namespace(&self) -> &str {
        "test"
    }
    async fn contribute(
        &self,
        _: &ToolContributionContext,
    ) -> GovernanceResult<Vec<ContributedTool>> {
        Ok(vec![ContributedTool::DirectWrite(self.0.clone())])
    }
}

struct World {
    _directory: tempfile::TempDir,
    path: std::path::PathBuf,
    store: SqliteStore,
    context: Context,
    tool: Arc<Mutate>,
}

/// A file-backed store (so a second connection can seed barriers, as the
/// tolerant TypeScript import does) with run `RUN` of chat `CHAT` claimed.
async fn world() -> World {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("barriers.db");
    let store = SqliteStore::open(&path, 8).unwrap();
    store
        .create_chat(CHAT, None, json!({}), now())
        .await
        .unwrap();
    let task = SubmitTask {
        task_id: RUN.into(),
        kind: "chat.turn".into(),
        scope_id: CHAT.into(),
        payload: json!({"chatId": CHAT}),
        priority: 0,
        now: now(),
        available_at: None,
        depends_on: vec![],
        parent_task_id: None,
        exclusive_scope: false,
    };
    store.submit(task).await.unwrap();
    let claim = ClaimInput {
        owner_id: "worker".into(),
        now: now(),
        ttl_ms: 1_000_000_000,
        kinds: vec![],
        busy_scopes: vec![],
        max_attempts: 3,
    };
    let claimed = store
        .claim_with_concurrency(claim, 1)
        .await
        .unwrap()
        .expect("claimed");
    let context = Context {
        task: claimed.task,
        attempt_id: claimed.lease.attempt_id.clone(),
        cancellation: CancellationToken::new(),
        interruption: CancellationToken::new(),
        suspension: Arc::new(tokio::sync::Mutex::new(None)),
        store: store.clone(),
        fence: claimed.lease.fence(),
        runtime: Arc::new(agentkit_engine::SystemRuntime),
        event_gate: Arc::new(tokio::sync::Mutex::new(())),
    };
    let definition = serde_json::from_value(json!({
        "name": "mutate", "version": "1", "effect": "write",
        "capability": "fs.write", "description": "mutate",
        "inputSchema": {"type": "object"},
    }))
    .unwrap();
    let tool = Arc::new(Mutate {
        definition,
        runs: Arc::new(AtomicUsize::new(0)),
    });
    World {
        _directory: directory,
        path,
        store,
        context,
        tool,
    }
}

impl World {
    /// Seed a chat-scoped barrier for `mutate` with these arguments, left by
    /// an imported execution `source`. Returns its id.
    fn barrier(&self, source: &str) -> String {
        let args = args_sha256(&input());
        let barrier = UncertaintyBarrierRecord {
            barrier_id: barrier_id_of(Some(source), None, "mutate", Some(&args)),
            chat_id: Some(CHAT.into()),
            run_id: None,
            tool_name: "mutate".into(),
            args_sha256: Some(args),
            args_hash_provenance: ArgsHashProvenance::Canonical,
            reason: format!("imported execution {source} could not be placed"),
        };
        let db = rusqlite::Connection::open(&self.path).unwrap();
        assert!(agentkit_store::insert_barrier(&db, &barrier).unwrap());
        barrier.barrier_id
    }

    fn durable(&self, approvals: HashMap<String, ApprovalGrant>) -> Arc<DurableExecution> {
        Arc::new(DurableExecution {
            context: self.context.clone(),
            chat_id: CHAT.into(),
            is_mutating: None,
            approvals: Arc::new(approvals),
            standing: None,
        })
    }

    fn contribution(&self, durable: Arc<DurableExecution>) -> ToolContributionContext {
        let port: Arc<dyn DurableExecutionPort> = durable;
        ToolContributionContext {
            chat_id: Some(CHAT.into()),
            run_id: RUN.into(),
            scope_id: None,
            actor: Value::Null,
            actor_id: None,
            bindings: vec![],
            limits: ToolLimits::default(),
            cancellation: CancellationToken::new(),
            has_primary_binding: true,
            timeout_ms: 30_000,
            durable: Some(port),
        }
    }

    /// One attempt calls `mutate` as `call`; returns the result and the
    /// request it parked, if any.
    async fn call(
        &self,
        durable: Arc<DurableExecution>,
        call: &str,
    ) -> (ToolResult, Option<String>) {
        let contribution = self.contribution(durable);
        let contributors: Vec<Arc<dyn ToolContributor>> =
            vec![Arc::new(Contributor(self.tool.clone()))];
        let staged = stage_tools(&contributors, &[], contribution).await.unwrap();
        let slot = Arc::new(SuspendSlot::default());
        let context = ToolContext {
            run_id: RUN.into(),
            tool_call_id: Some(call.into()),
            attempt_id: Some(self.context.attempt_id.clone()),
            chat_id: Some(CHAT.into()),
            scope_id: None,
            user_id: None,
            bindings: vec![],
            limits: ToolLimits::default(),
            cancellation: CancellationToken::new(),
            dispatch_guard: None,
            metadata: Default::default(),
            suspension: Some(slot.clone()),
        };
        let tool = staged.registry.get("mutate").expect("staged");
        let result = tool.execute(context, input()).await.unwrap();
        let shown = format!("{slot:?}");
        (result, shown.contains("request_id").then_some(shown))
    }

    fn runs(&self) -> usize {
        self.tool.runs.load(Ordering::SeqCst)
    }

    /// The owner says yes to the uncertainty card `call` parked about
    /// `generation`: the request the host records, and the grant built
    /// from it.
    async fn yes(&self, call: &str, generation: &str) -> ApprovalGrant {
        let probe = self.durable(HashMap::new());
        let context_sha256 = probe.execution_context_sha256(
            &self.contribution(probe.clone()),
            &self.tool.definition,
            None,
        );
        let request_id = format!("hreq:{RUN}:{call}:uncertain:{generation}");
        let content = json!({
            "prompt": "approve?",
            "toolName": "mutate",
            "kind": "approval",
            "argsSha256": args_sha256(&input()),
            "contextSha256": context_sha256,
            "approvalGeneration": generation,
            "cardKind": "uncertain",
        });
        let request = CreateAgentInput {
            input_id: request_id.clone(),
            session_id: CHAT.into(),
            kind: AgentInputKind::HumanRequest,
            content_hash: agentkit_kernel::sha256_hex(content.to_string()),
            content,
            target_task_id: Some(RUN.into()),
            on_settled: None,
            attempt_id: Some(self.context.attempt_id.clone()),
            tool_call_id: Some(call.into()),
            expires_at: None,
        };
        self.store.create_input(request, now()).await.unwrap();
        let answer_id = format!("answer:{request_id}");
        let approved = AgentInputPatch {
            applied_task_id: None,
            applied_message_id: Some(format!("approval:{answer_id}")),
            answer_id: Some(answer_id),
        };
        let (from, to) = (vec![AgentInputStatus::Pending], AgentInputStatus::Answered);
        self.store
            .transition_input(request_id.clone(), from, to, approved, now())
            .await
            .unwrap();
        ApprovalGrant {
            request_id,
            args_sha256: args_sha256(&input()),
            uncertainty_generation: Some(generation.into()),
            context_sha256: Some(context_sha256),
            uncertainty_card: true,
        }
    }
}

#[tokio::test]
async fn two_matching_barriers_end_the_call_naming_both() {
    let world = world().await;
    let first = world.barrier("imported-1");
    let second = world.barrier("imported-2");
    let (refused, parked) = world.call(world.durable(HashMap::new()), "call-1").await;
    assert_eq!(parked, None, "no answer could open both: never an ask");
    let data = refused.data.clone().unwrap();
    assert_eq!(data["errorCode"], "tool_result_uncertain", "{refused:?}");
    let named: Vec<&str> = data["unresolvedGenerations"]
        .as_array()
        .expect("every unresolved generation is listed")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(named.contains(&first.as_str()), "{named:?}");
    assert!(named.contains(&second.as_str()), "{named:?}");
    assert_eq!(world.runs(), 0);
}

#[tokio::test]
async fn an_operator_resolves_one_barrier_and_one_yes_runs_the_call_once() {
    let world = world().await;
    let first = world.barrier("imported-1");
    let second = world.barrier("imported-2");
    let (refused, _) = world.call(world.durable(HashMap::new()), "call-1").await;
    let named = refused.data.unwrap()["approvalGeneration"].clone();
    let lifted = world
        .store
        .resolve_uncertainty_barrier(CHAT.into(), named.as_str().unwrap().into())
        .await
        .expect("an operator resolves a barrier by the id the refusal names");
    let left = if lifted.barrier_id == first {
        second
    } else {
        first
    };
    let (_, parked) = world.call(world.durable(HashMap::new()), "call-2").await;
    let card = parked.expect("one barrier left: the call asks about it");
    assert!(card.contains(&format!(":uncertain:{left}")), "{card}");
    assert_eq!(world.runs(), 0, "never unasked");
    let grant = world.yes("call-2", &left).await;
    let answered = world.durable(HashMap::from([("call-2".to_owned(), grant)]));
    let (done, parked) = world.call(answered, "call-2").await;
    assert!(done.ok, "{done:?}");
    assert_eq!(parked, None);
    assert_eq!(world.runs(), 1, "one yes, one run");
}
