//! The world of the pack tests: the workspace service over the in-memory
//! repository twin and the fake backend on one manual clock, a fake binding
//! resolver, a recording export sink, and a durable port that only answers
//! approvals (direct tool calls set the execution id themselves; the staged
//! end-to-end test uses the real ledger).
#![allow(dead_code)]

pub mod ledger;

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use agentkit_core::{
    CancellationToken, CoreError, Tool, ToolContext, ToolDefinition, ToolLimits, ToolResult,
};
use agentkit_governance::{
    Admission, ApprovalGrant, CommandCompletion, ContributedTool, DurableExecutionPort,
    EXECUTION_ID_METADATA_KEY, GovernanceResult, PreGuard, StandingApprovalPolicy,
    ToolContributionContext, ToolContributor, ToolGuardContext, args_sha256,
};
use agentkit_testkit::{FakeSandboxBackend, InMemoryWorkspaceRepository, ManualClock, Program};
use agentkit_toolpacks_sandbox::{
    ExportContext, ExportKind, ExportReceipt, ExportSinkError, SandboxBinding,
    SandboxBindingResolver, SandboxEnvironment, SandboxExportSink, SandboxMode, SandboxPackOptions,
    SandboxPermissionGuard, SandboxToolPack,
};
use agentkit_workspaces::{
    AgentId, Binding, ExportFile, FileMode, FileRef, OpContext, OperationId, Pause, RelPath,
    ServiceConfig, Sha256, WorkspaceKey, WorkspaceKind, WorkspaceService, WriteRequest,
};
use async_trait::async_trait;
use serde_json::{Map, Value, json};

pub const IMAGE: &str = "sha256:image-1";
pub const RUN: &str = "run-1";
pub const CHAT: &str = "chat-1";
pub const ECHO: &str = "echo hi";
pub const EXIT_3: &str = "exit 3";
pub const SLEEP: &str = "sleep infinity";
pub const CURL: &str = "curl https://example.com/";
pub const CURL_OTHER: &str = "curl https://other.example.org/";
pub const MARK: &str = "printf x >> mark.txt";

/// Lets virtual time pass instead of sleeping.
pub struct ClockPause(pub Arc<ManualClock>);

#[async_trait]
impl Pause for ClockPause {
    async fn pause(&self, ms: u64) {
        tokio::task::yield_now().await;
        self.0.advance(i64::try_from(ms).unwrap_or(i64::MAX));
    }
}

/// The attachment the host pins, changeable mid-test.
#[derive(Default)]
pub struct FakeResolver {
    pub binding: Mutex<Option<SandboxBinding>>,
}

impl FakeResolver {
    pub fn set(&self, binding: Option<SandboxBinding>) {
        *self.binding.lock().unwrap() = binding;
    }
}

#[async_trait]
impl SandboxBindingResolver for FakeResolver {
    async fn binding(
        &self,
        _: &ToolContributionContext,
    ) -> GovernanceResult<Option<SandboxBinding>> {
        Ok(self.binding.lock().unwrap().clone())
    }
}

/// Records every export it receives.
#[derive(Default)]
pub struct RecordingSink {
    pub exports: Mutex<Vec<(ExportContext, ExportFile, String)>>,
}

impl RecordingSink {
    pub fn count(&self) -> usize {
        self.exports.lock().unwrap().len()
    }
}

#[async_trait]
impl SandboxExportSink for RecordingSink {
    async fn export(
        &self,
        context: &ExportContext,
        file: ExportFile,
        title: &str,
    ) -> Result<ExportReceipt, ExportSinkError> {
        let mut exports = self.exports.lock().unwrap();
        exports.push((context.clone(), file, title.to_owned()));
        Ok(ExportReceipt {
            file_id: format!("file-{}", exports.len()),
            kind: ExportKind::File,
        })
    }
}

struct NoCompletion;

impl CommandCompletion for NoCompletion {
    fn finish(self: Box<Self>, _: Option<String>) {}
}

/// A durable port that only answers approvals: the grants a resumed
/// attempt carries. Each staging gets a snapshot.
#[derive(Default)]
pub struct Grants {
    approvals: Mutex<HashMap<String, ApprovalGrant>>,
}

impl Grants {
    /// The owner answered the ask of `call` (about `args`) with yes.
    pub fn approve(&self, call: &str, args: &Value) {
        self.insert(call, args, None);
    }

    /// The owner answered an uncertain re-run of `call`.
    pub fn approve_rerun(&self, call: &str, args: &Value) {
        self.insert(call, args, Some("exec-earlier".into()));
    }

    /// The answers as one attempt sees them.
    pub fn snapshot(&self) -> Grants {
        let approvals = self.approvals.lock().unwrap().clone();
        Grants {
            approvals: Mutex::new(approvals),
        }
    }

    fn insert(&self, call: &str, args: &Value, generation: Option<String>) {
        let grant = ApprovalGrant {
            request_id: format!("hreq:{RUN}:{call}"),
            args_sha256: args_sha256(args),
            uncertainty_card: generation.is_some(),
            uncertainty_generation: generation,
            context_sha256: None,
        };
        self.approvals
            .lock()
            .unwrap()
            .insert(call.to_owned(), grant);
    }
}

#[async_trait]
impl DurableExecutionPort for Grants {
    fn approval_grant(&self, tool_call_id: &str) -> Option<ApprovalGrant> {
        self.approvals.lock().unwrap().get(tool_call_id).cloned()
    }
    fn standing_policy(&self) -> Option<Arc<dyn StandingApprovalPolicy>> {
        None
    }
    fn execution_context_sha256(
        &self,
        _: &ToolContributionContext,
        _: &ToolDefinition,
        _: Option<&Value>,
    ) -> String {
        String::new()
    }
    async fn has_active_standing_grant(&self, _: &str) -> Result<bool, CoreError> {
        Ok(false)
    }
    async fn before_guards(
        &self,
        _: &ToolContext,
        _: &ToolGuardContext,
        _: &ToolDefinition,
        _: Option<&Value>,
        _: &Value,
    ) -> Result<PreGuard, CoreError> {
        Ok(PreGuard::Proceed)
    }
    async fn admit(
        &self,
        _: &ToolContext,
        _: &ToolGuardContext,
        _: &ToolDefinition,
        _: Option<&Value>,
        _: &Value,
    ) -> Result<Admission, CoreError> {
        Ok(Admission::Skipped)
    }
    async fn record_schema_refusal(
        &self,
        _: &ToolContext,
        _: &ToolGuardContext,
        _: &ToolDefinition,
        _: Option<&Value>,
        _: &Value,
        _: &str,
    ) {
    }
    async fn complete(&self, _: &str, _: &ToolResult) -> Result<(), CoreError> {
        Ok(())
    }
    async fn complete_not_started(&self, _: &str, _: &ToolResult) -> Result<(), CoreError> {
        Ok(())
    }
    fn register_command_completion(&self) -> Box<dyn CommandCompletion> {
        Box::new(NoCompletion)
    }
}

pub fn rel(text: &str) -> RelPath {
    RelPath::parse(text).unwrap()
}

/// A project chat attachment: folder `chats/c1`.
pub fn chat_binding(mode: SandboxMode) -> SandboxBinding {
    SandboxBinding {
        binding: Binding {
            key: WorkspaceKey::new("chats:project:p1"),
            kind: WorkspaceKind::ProjectChats,
            owner_scope: "owner:1".into(),
            folder: Some(rel("chats/c1")),
        },
        agent_id: AgentId::new("chat-agent"),
        mode,
    }
}

/// An agent home attachment.
pub fn home_binding(mode: SandboxMode) -> SandboxBinding {
    SandboxBinding {
        binding: Binding {
            key: WorkspaceKey::new("agent:a1"),
            kind: WorkspaceKind::AgentHome,
            owner_scope: "owner:1".into(),
            folder: None,
        },
        agent_id: AgentId::new("a1"),
        mode,
    }
}

pub struct World {
    pub clock: Arc<ManualClock>,
    pub repo: Arc<InMemoryWorkspaceRepository>,
    pub fake: FakeSandboxBackend,
    pub service: Arc<WorkspaceService>,
    pub resolver: Arc<FakeResolver>,
    pub sink: Arc<RecordingSink>,
    pub pack: Arc<SandboxToolPack>,
    pub guard: Arc<SandboxPermissionGuard>,
    pub grants: Arc<Grants>,
    puts: std::sync::atomic::AtomicU64,
}

fn script(fake: &FakeSandboxBackend) {
    fake.script(ECHO, Program::exit(0).stdout("hi\n"))
        .script(EXIT_3, Program::exit(3).stdout("bad input\n"))
        .script(SLEEP, Program::hang())
        .script(CURL, Program::connect("example.com", 443))
        .script(CURL_OTHER, Program::connect("other.example.org", 443))
        .script(MARK, Program::exit(0).append("mark.txt", "x"));
}

impl World {
    /// A world whose run is attached to `binding` (or to nothing).
    pub fn new(binding: Option<SandboxBinding>) -> Self {
        let clock = ManualClock::new(1_000_000);
        let fake = FakeSandboxBackend::with_clock(clock.clone());
        script(&fake);
        let repo = Arc::new(InMemoryWorkspaceRepository::new());
        let service = WorkspaceService::new(
            repo.clone(),
            Arc::new(fake.clone()),
            clock.clone(),
            ServiceConfig::new(IMAGE),
        )
        .expect("valid config")
        .with_pause(Arc::new(ClockPause(clock.clone())));
        let service = Arc::new(service);
        let resolver = Arc::new(FakeResolver::default());
        resolver.set(binding);
        let sink = Arc::new(RecordingSink::default());
        let pack = Arc::new(Self::pack(&service, &resolver, &sink));
        let guard = Arc::new(pack.permission_guard());
        Self {
            clock,
            repo,
            fake,
            service,
            resolver,
            sink,
            pack,
            guard,
            grants: Arc::new(Grants::default()),
            puts: std::sync::atomic::AtomicU64::new(0),
        }
    }

    fn pack(
        service: &Arc<WorkspaceService>,
        resolver: &Arc<FakeResolver>,
        sink: &Arc<RecordingSink>,
    ) -> SandboxToolPack {
        SandboxToolPack::new(SandboxPackOptions {
            service: service.clone(),
            resolver: resolver.clone(),
            export_sink: sink.clone(),
            authority: None,
            environment: SandboxEnvironment::new("2026.11 (sha256:image-1)"),
        })
    }

    /// The same host after a restart: a new pack (nothing remembered) over
    /// the same service, resolver and sink.
    pub fn restart_pack(&mut self) {
        self.pack = Arc::new(Self::pack(&self.service, &self.resolver, &self.sink));
        self.guard = Arc::new(self.pack.permission_guard());
    }

    /// A staging context of the run; its durable port answers `grants`.
    pub fn contribution(&self) -> ToolContributionContext {
        // One immutable set of answers per attempt, as the real ledger.
        let durable: Arc<dyn DurableExecutionPort> = Arc::new(self.grants.snapshot());
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
            durable: Some(durable),
        }
    }

    /// Stage the tools (one attempt) and index them by name.
    pub async fn stage(&self) -> BTreeMap<String, Arc<dyn Tool>> {
        let tools = self
            .pack
            .contribute(&self.contribution())
            .await
            .expect("contribute");
        tools
            .into_iter()
            .map(|tool| match tool {
                ContributedTool::Read(tool) | ContributedTool::DirectWrite(tool) => tool,
                ContributedTool::Write(_) => panic!("sandbox tools are direct writes"),
            })
            .map(|tool| (tool.definition().name.clone(), tool))
            .collect()
    }

    /// The guard's verdict on `tool` called with `args`.
    pub async fn verdict(
        &self,
        tool: &dyn Tool,
        args: Value,
    ) -> agentkit_governance::ToolGuardVerdict {
        use agentkit_governance::ToolGuard;
        let context = ToolGuardContext {
            contribution: self.contribution(),
            namespace: "sandbox".into(),
            tool: tool.definition().clone(),
            arguments: Some(args),
        };
        self.guard
            .can_execute(&context)
            .await
            .expect("guard answers")
    }

    /// Write `content` to `path` (below the chat folder) as an owner-side
    /// operation, outside the tools.
    pub async fn put(&self, binding: &SandboxBinding, path: &str, content: &str) {
        let n = self.puts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let op = OpContext::new(
            OperationId::new(format!("put-{n}")),
            Sha256::of(content.as_bytes()),
            "owner-run",
            binding.agent_id.clone(),
        );
        let request = WriteRequest {
            file: FileRef::workspace(rel(path)),
            bytes: content.as_bytes().to_vec(),
            mode: FileMode::REGULAR,
        };
        let outcome = self
            .service
            .write_file(&op, &binding.binding, request)
            .await
            .expect("put");
        assert!(!outcome.settlement.is_uncertain());
        self.clock.advance(1);
    }
}

impl World {
    /// Let spawned work run until a mutating operation is open.
    pub async fn until_busy(&self) {
        use agentkit_workspaces::WorkspaceRepository;
        for _ in 0..100_000 {
            if !self.repo.list_open_operations().await.unwrap().is_empty() {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("no operation ever opened");
    }
}

/// A tool call of the run; `execution` is the id the ledger admitted it
/// under (`None`: not admitted).
pub fn call(call_id: &str, execution: Option<&str>) -> ToolContext {
    let mut metadata = Map::new();
    if let Some(id) = execution {
        metadata.insert(EXECUTION_ID_METADATA_KEY.into(), json!(id));
    }
    ToolContext {
        run_id: RUN.into(),
        tool_call_id: Some(call_id.into()),
        attempt_id: None,
        chat_id: Some(CHAT.into()),
        scope_id: None,
        user_id: None,
        bindings: vec![],
        limits: ToolLimits::default(),
        cancellation: CancellationToken::new(),
        dispatch_guard: None,
        metadata,
        suspension: None,
    }
}

/// What the model reads.
pub fn text(result: &ToolResult) -> String {
    result
        .model_data
        .as_ref()
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

pub fn data(result: &ToolResult) -> Value {
    result.data.clone().unwrap_or(Value::Null)
}

/// True for the ledger's not-started shape.
pub fn not_started(result: &ToolResult) -> bool {
    let data = data(result);
    !result.ok
        && data["errorCode"] == "command_not_started"
        && data["phase"] == "before_spawn"
        && data["started"] == false
}
