//! The real durable execution ledger over an in-memory SQLite store
//! (dev-dependency only, R11): a claimed run, one attempt's staging through
//! governance, and the host's record of an owner's answer.

use std::collections::HashMap;
use std::sync::Arc;

use agentkit_core::{CancellationToken, SuspendSlot, Tool, ToolContext, ToolLimits, ToolResult};
use agentkit_governance::{
    ApprovalGrant, ContributedTool, DurableExecution, DurableExecutionPort,
    ToolContributionContext, ToolContributor, ToolGuard, args_sha256, stage_tools,
};
use agentkit_store::{
    AgentInputKind, AgentInputPatch, AgentInputStatus, ClaimInput, CreateAgentInput, SqliteStore,
    SubmitTask,
};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use super::{CHAT, RUN, World};

pub type Context = agentkit_engine::ExecutionContext<SqliteStore>;

fn now() -> i64 {
    agentkit_engine::Runtime::now_ms(&agentkit_engine::SystemRuntime)
}

/// The run `RUN` of chat `CHAT`, submitted and claimed by one worker.
pub async fn claimed() -> (SqliteStore, Context) {
    let store = SqliteStore::memory(8).unwrap();
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
        suspension: Arc::new(Mutex::new(None)),
        store: store.clone(),
        fence: claimed.lease.fence(),
        runtime: Arc::new(agentkit_engine::SystemRuntime),
        event_gate: Arc::new(Mutex::new(())),
    };
    (store, context)
}

/// One attempt's ledger, carrying `approvals` (the answers it resumes).
pub fn durable(
    context: &Context,
    approvals: HashMap<String, ApprovalGrant>,
) -> Arc<DurableExecution> {
    Arc::new(DurableExecution {
        context: context.clone(),
        chat_id: CHAT.into(),
        is_mutating: None,
        approvals: Arc::new(approvals),
        standing: None,
    })
}

pub fn contribution(durable: &Arc<DurableExecution>) -> ToolContributionContext {
    let port: Arc<dyn DurableExecutionPort> = durable.clone();
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

/// One attempt: stage the pack and its guard through governance.
pub async fn attempt(world: &World, durable: &Arc<DurableExecution>, tool: &str) -> Arc<dyn Tool> {
    let [tool] = attempt_tools(world, durable, [tool]).await;
    tool
}

/// One attempt's tools `names`, from a single staging (as one turn
/// attempt's calls share it).
pub async fn attempt_tools<const N: usize>(
    world: &World,
    durable: &Arc<DurableExecution>,
    names: [&str; N],
) -> [Arc<dyn Tool>; N] {
    let contributors: Vec<Arc<dyn ToolContributor>> = vec![world.pack.clone()];
    let guards: Vec<Arc<dyn ToolGuard>> = vec![world.guard.clone()];
    let staged = stage_tools(&contributors, &guards, contribution(durable))
        .await
        .unwrap();
    names.map(|name| staged.registry.get(name).expect("staged"))
}

pub fn tool_call(context: &Context, call: &str, slot: Option<Arc<SuspendSlot>>) -> ToolContext {
    ToolContext {
        attempt_id: Some(context.attempt_id.clone()),
        suspension: slot,
        ..super::call(call, None)
    }
}

pub async fn execute(tool: &Arc<dyn Tool>, context: ToolContext, input: Value) -> ToolResult {
    tool.execute(context, input).await.expect("no error")
}

/// The unstaged pack tool `name` and the effect context it carries.
pub async fn pack_tool(
    world: &World,
    durable: &Arc<DurableExecution>,
    name: &str,
) -> (Arc<dyn Tool>, Option<Value>) {
    let tools = world.pack.contribute(&contribution(durable)).await.unwrap();
    let tool = tools
        .into_iter()
        .map(|tool| match tool {
            ContributedTool::Read(tool) | ContributedTool::DirectWrite(tool) => tool,
            ContributedTool::Write(_) => panic!("sandbox tools are direct writes"),
        })
        .find(|tool| tool.definition().name == name)
        .expect("contributed");
    let effect = tool.effect_context();
    (tool, effect)
}

/// The execution-context hash an ask of `name` carries.
pub async fn context_hash(world: &World, context: &Context, name: &str) -> String {
    let probe = durable(context, HashMap::new());
    let (tool, effect) = pack_tool(world, &probe, name).await;
    probe.execution_context_sha256(&contribution(&probe), tool.definition(), effect.as_ref())
}

/// What one answered request binds.
pub struct Answer<'a> {
    pub call: &'a str,
    pub tool: &'a str,
    pub input: &'a Value,
    /// The uncertain generation the card asked about, if any.
    pub generation: Option<&'a str>,
    pub context_sha256: String,
}

/// What the host records when the run parks on an ask and the owner says
/// yes, and the grant it builds from that answer.
pub async fn answer(store: &SqliteStore, context: &Context, answer: Answer<'_>) -> ApprovalGrant {
    let request_id = match answer.generation {
        Some(generation) => format!("hreq:{RUN}:{}:uncertain:{generation}", answer.call),
        None => format!("hreq:{RUN}:{}", answer.call),
    };
    let mut content = json!({
        "prompt": "approve?",
        "toolName": answer.tool,
        "kind": "approval",
        "argsSha256": args_sha256(answer.input),
        "contextSha256": answer.context_sha256,
    });
    // A generation is asked about on the uncertainty card only.
    if let Some(generation) = answer.generation {
        content["approvalGeneration"] = json!(generation);
        content["cardKind"] = json!("uncertain");
    }
    let request = CreateAgentInput {
        input_id: request_id.clone(),
        session_id: CHAT.into(),
        kind: AgentInputKind::HumanRequest,
        content_hash: agentkit_kernel::sha256_hex(content.to_string()),
        content,
        target_task_id: Some(RUN.into()),
        on_settled: None,
        attempt_id: Some(context.attempt_id.clone()),
        tool_call_id: Some(answer.call.into()),
        expires_at: None,
    };
    store.create_input(request, now()).await.unwrap();
    let answer_id = format!("answer:{request_id}");
    let approved = AgentInputPatch {
        applied_task_id: None,
        applied_message_id: Some(format!("approval:{answer_id}")),
        answer_id: Some(answer_id),
    };
    let (from, to) = (vec![AgentInputStatus::Pending], AgentInputStatus::Answered);
    store
        .transition_input(request_id.clone(), from, to, approved, now())
        .await
        .unwrap();
    ApprovalGrant {
        request_id,
        args_sha256: args_sha256(answer.input),
        uncertainty_generation: answer.generation.map(str::to_owned),
        context_sha256: Some(answer.context_sha256),
        uncertainty_card: answer.generation.is_some(),
    }
}

/// The request a parked call left in its slot (`None`: it did not park).
pub fn parked(slot: &SuspendSlot) -> Option<String> {
    let shown = format!("{slot:?}");
    shown.contains("request_id").then_some(shown)
}
