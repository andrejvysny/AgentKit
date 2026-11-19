//! Runtime composition. The order below is load-bearing and mirrors what the
//! binding did inline: lanes and authority first, then the provider and
//! profile registries, the executor, the host, the session service, the
//! late binding of the authority and lanes, recovery, and only then the
//! worker.
//!
//! A model provider is optional, but never silently (ADR 0026, the open
//! question on a native-lane-only embedder): `build()` accepts a runtime
//! with no provider only when the model loop can never be the execution
//! owner of an admitted turn — a native lane is configured, so the dispatch
//! wrapper owns the turns it accepts, or the consumer registered its own
//! executor for every turn kind the builder would otherwise serve. Every
//! other configuration is refused by name, and a turn that nevertheless
//! reaches the model loop of a providerless runtime settles failed with the
//! [`RefusingExecutor`]'s deterministic code — never a panic.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use agentkit_agents::AdvisorEventSink;
use agentkit_chat::{AgentExecutor, AgentExecutorOptions, ProviderRegistry};
use agentkit_chat::{AgentHost, HostOptions};
use agentkit_core::permission::CodingMode;
use agentkit_core::{ChatProvider, RunHooks, ToolRegistry};
use agentkit_engine::{
    ExecutionFailure, RetrySafety, SharedRuntime, SystemRuntime, TaskExecutor, VerificationHook,
};
use agentkit_governance::{CodingPermissionAuthority, ToolContributor, ToolGuard};
use agentkit_memory::{
    Embedder, MemoryRepository, MemoryService, Reranker, RetrievalIndex, Timestamp,
};
use agentkit_native_exec::{NativeAgentExecutor, NativeDispatchExecutor};
use agentkit_providers::NativeProviderFactory;
use agentkit_sessions::{
    AgentSessionService, AgentSessionServiceDeps, CHAT_TURN_TASK_KIND, ContentAuthority,
    ProfileRegistry,
};
use async_trait::async_trait;
use serde_json::Value;

use crate::authority::{BindingAuthority, BindingPlanPolicy, JsToolGuard, PlanConfiguration};
use crate::error::RuntimeError;
use crate::lanes::advisor::{AdvisorLane, AdvisorLaneContributor, LaneAdvisorTools};
use crate::lanes::coding::{
    CodingLane, CodingLaneContributor, CodingLaneGuard, LaneWorkspaceResolver,
};
use crate::lanes::lsp::{LspLane, LspLaneContributor};
use crate::lanes::memory::{
    MemoryLane, spawn_memory_indexer, spawn_memory_sweeper, spawn_usage_flusher,
};
use crate::lanes::memory_capture::{MEMORY_EXTRACT_TASK_KIND, MemoryCaptureLane};
use crate::lanes::native::NativeLane;
use crate::lanes::output::{OutputLaneContributor, spawn_sweeper};
use crate::lanes::subagent::{SubagentLane, SubagentLaneContributor, SubagentLaneGuard};
use crate::runtime::AgentRuntime;

/// Workers for root sessions and their delegating parents; the subagent
/// `maxConcurrency` is added on top at construction.
const BASE_WORKERS: usize = 12;

/// Governs externally contributed (JS) tools with the internal coding
/// authority: the guard joins the front of the guard chain.
pub struct ExternalToolsPolicy {
    pub configured_mode: CodingMode,
    pub plan: PlanConfiguration,
}

pub struct AgentRuntimeBuilder {
    store: agentkit_store::SqliteStore,
    clock: SharedRuntime,
    default_provider: Option<Arc<dyn ChatProvider>>,
    additional_providers: Vec<Arc<dyn ChatProvider>>,
    extra_contributors: Vec<Arc<dyn ToolContributor>>,
    external_tools: Option<ExternalToolsPolicy>,
    content_authority: Option<Arc<dyn ContentAuthority<agentkit_store::StoreError>>>,
    profiles: Vec<Value>,
    subagent_max_concurrency: i64,
    plan: PlanConfiguration,
    default_provider_id: Option<String>,
    default_model: Option<String>,
    advisor_sink: Option<Arc<dyn AdvisorEventSink>>,
    lease_ttl_ms: i64,
    verification: Option<Arc<dyn VerificationHook<agentkit_store::SqliteStore>>>,
    turn_kinds: Vec<String>,
    extra_executors: Vec<(String, Arc<dyn TaskExecutor<agentkit_store::SqliteStore>>)>,
    native_lane: Option<NativeLane>,
    memory: Option<(Arc<dyn MemoryRepository>, Arc<dyn RetrievalIndex>)>,
    memory_sweep: bool,
    embedder: Option<Arc<dyn Embedder>>,
    reranker: Option<Arc<dyn Reranker>>,
    memory_extractor: Option<(Arc<dyn ChatProvider>, String)>,
    output: Option<OutputConfig>,
}

/// The output store's configuration (roadmap 2.1, ADR 0034).
struct OutputConfig {
    root: std::path::PathBuf,
    inline_limit: usize,
    tool_limits: Vec<(String, usize)>,
}

impl AgentRuntimeBuilder {
    pub(crate) fn new(store: agentkit_store::SqliteStore) -> Self {
        Self {
            store,
            clock: Arc::new(SystemRuntime),
            default_provider: None,
            additional_providers: Vec::new(),
            extra_contributors: Vec::new(),
            external_tools: None,
            content_authority: None,
            profiles: Vec::new(),
            subagent_max_concurrency: agentkit_agents::SubagentConfig::default()
                .limits
                .max_concurrency,
            plan: PlanConfiguration {
                tool_names: vec!["submit_plan".to_owned()],
                granted_mode: CodingMode::Ask,
            },
            default_provider_id: None,
            default_model: None,
            advisor_sink: None,
            lease_ttl_ms: HostOptions::default().lease_ttl_ms,
            verification: None,
            turn_kinds: Vec::new(),
            extra_executors: Vec::new(),
            native_lane: None,
            memory: None,
            memory_sweep: false,
            embedder: None,
            reranker: None,
            memory_extractor: None,
            output: None,
        }
    }

    pub fn clock(mut self, clock: SharedRuntime) -> Self {
        self.clock = clock;
        self
    }

    /// The provider that serves turns when a session pins none. Optional: a
    /// runtime with no model loop at all is legal — [`AgentRuntimeBuilder::build`]
    /// states when, and what happens to a turn that then needs one.
    pub fn default_provider(mut self, provider: Arc<dyn ChatProvider>) -> Self {
        self.default_provider = Some(provider);
        self
    }

    /// Extra routable providers, registered after the default one.
    pub fn additional_providers(mut self, providers: Vec<Arc<dyn ChatProvider>>) -> Self {
        self.additional_providers = providers;
        self
    }

    /// Contributors placed ahead of the built-in lane contributors, in the
    /// given order.
    pub fn extra_contributors(mut self, contributors: Vec<Arc<dyn ToolContributor>>) -> Self {
        self.extra_contributors = contributors;
        self
    }

    pub fn external_tools(mut self, policy: ExternalToolsPolicy) -> Self {
        self.external_tools = Some(policy);
        self
    }

    pub fn content_authority(
        mut self,
        authority: Arc<dyn ContentAuthority<agentkit_store::StoreError>>,
    ) -> Self {
        self.content_authority = Some(authority);
        self
    }

    /// Profile definitions merged over the built-ins.
    pub fn profiles(mut self, profiles: Vec<Value>) -> Self {
        self.profiles = profiles;
        self
    }

    /// START-TIME subagent `maxConcurrency`: sizes the worker pool and is
    /// the ceiling later subagent configuration is clamped to.
    pub fn subagent_max_concurrency(mut self, max_concurrency: i64) -> Self {
        self.subagent_max_concurrency = max_concurrency;
        self
    }

    pub fn plan(mut self, plan: PlanConfiguration) -> Self {
        self.plan = plan;
        self
    }

    /// Session defaults: the provider id and model new sessions start with.
    pub fn session_defaults(mut self, provider_id: Option<String>, model: Option<String>) -> Self {
        self.default_provider_id = provider_id;
        self.default_model = model;
        self
    }

    pub fn advisor_sink(mut self, sink: Arc<dyn AdvisorEventSink>) -> Self {
        self.advisor_sink = Some(sink);
        self
    }

    /// Task lease TTL in milliseconds (host default when unset).
    pub fn lease_ttl_ms(mut self, lease_ttl_ms: i64) -> Self {
        self.lease_ttl_ms = lease_ttl_ms;
        self
    }

    /// Post-run verification hook run by the host after a turn settles.
    pub fn verification(
        mut self,
        hook: Arc<dyn VerificationHook<agentkit_store::SqliteStore>>,
    ) -> Self {
        self.verification = Some(hook);
        self
    }

    /// Additional task kinds served by the turn executor (the chat executor,
    /// or the native dispatch wrapper when a native lane is configured).
    pub fn turn_kinds(mut self, kinds: Vec<String>) -> Self {
        self.turn_kinds = kinds;
        self
    }

    /// Register an executor for a task kind the runtime does not own.
    /// A kind the builder already serves (the chat turn kind, or one listed
    /// in `turn_kinds`, or registered twice) is refused at `build()` —
    /// except in one case: with no provider and no native lane, every turn
    /// kind the builder would serve is a refusal stand-in, and taking it
    /// over is exactly what a model-loop-less consumer is meant to do.
    pub fn executor(
        mut self,
        kind: impl Into<String>,
        executor: Arc<dyn TaskExecutor<agentkit_store::SqliteStore>>,
    ) -> Self {
        self.extra_executors.push((kind.into(), executor));
        self
    }

    /// Route turns whose frozen execution target names a vendor runtime
    /// through `agentkit-native-exec`: the turn executor becomes the
    /// dispatch wrapper (native target on the native executor, everything
    /// else on the model loop). The driver itself stays outside the runtime
    /// (ADR 0025): the consumer supplies the concrete ports (ADR 0026).
    pub fn native_lane(mut self, lane: NativeLane) -> Self {
        self.native_lane = Some(lane);
        self
    }

    /// Enable long-term memory: the record repository and the retrieval index
    /// the memory service drives (ADR 0035). The consumer names the concrete
    /// adapters, as with the store; without this call the runtime has no
    /// memory service.
    pub fn memory(
        mut self,
        repository: Arc<dyn MemoryRepository>,
        index: Arc<dyn RetrievalIndex>,
    ) -> Self {
        self.memory = Some((repository, index));
        self
    }

    /// Enable long-term memory on the runtime's own SQLite store: the
    /// record repository and the lexical retrieval index share its database,
    /// so a record and its index entry commit together. Retracted records are
    /// purged 30 days after their retraction (swept at build and then
    /// hourly, beside the output store's sweep). Replaces an earlier
    /// [`Self::memory`] call.
    pub fn sqlite_memory(mut self) -> Self {
        let repository = agentkit_store::SqliteMemoryRepository::new(self.store.clone());
        let index = agentkit_store::SqliteRetrievalIndex::new(self.store.clone());
        self.memory = Some((Arc::new(repository), Arc::new(index)));
        self.memory_sweep = true;
        self
    }

    /// The embedder memory search uses for vectors. Optional: without one,
    /// search runs lexical-only. Refused at `build()` unless `memory(..)` is
    /// also set.
    pub fn embedder(mut self, embedder: Arc<dyn Embedder>) -> Self {
        self.embedder = Some(embedder);
        self
    }

    /// The reranker applied to fused memory results. Optional: the default
    /// leaves the fused order unchanged. Refused at `build()` unless
    /// `memory(..)` is also set.
    pub fn reranker(mut self, reranker: Arc<dyn Reranker>) -> Self {
        self.reranker = Some(reranker);
        self
    }

    /// The background provider and model of memory auto-capture (roadmap
    /// 3.4, spec W2). Optional: without it no extraction task is ever
    /// queued. Refused at `build()` unless `memory(..)` is also set.
    pub fn memory_extractor_provider(
        mut self,
        provider: Arc<dyn ChatProvider>,
        model: impl Into<String>,
    ) -> Self {
        self.memory_extractor = Some((provider, model.into()));
        self
    }

    /// Turn the output store on (roadmap 2.1, ADR 0034): a tool result larger
    /// than the inline limit is stored before the model first sees it, the
    /// model reads a preview with a handle, and `read_output` and
    /// `search_output` page the rest. `root` is the private directory for the
    /// stored bytes (created 0700, files 0600); the host must choose one that
    /// is not inside any workspace. Stored outputs are removed with their
    /// chat and 30 days after the session's last activity (swept at build and
    /// then hourly, together with files no row names).
    /// Without this call results keep the envelope's byte budget.
    pub fn output_store(mut self, root: impl Into<std::path::PathBuf>) -> Self {
        self.output = Some(OutputConfig {
            root: root.into(),
            inline_limit: agentkit_core::DEFAULT_INLINE_LIMIT,
            tool_limits: Vec::new(),
        });
        self
    }

    /// Inline limit for every tool without its own (default 8 KiB, at least
    /// 1 KiB: [`Self::build`] refuses less). The effective limit is also
    /// held inside the context's result byte budget. Has no effect before
    /// [`Self::output_store`].
    pub fn output_inline_limit(mut self, bytes: usize) -> Self {
        if let Some(output) = &mut self.output {
            output.inline_limit = bytes;
        }
        self
    }

    /// Inline limit for one tool, overriding the default.
    pub fn output_tool_limit(mut self, tool: impl Into<String>, bytes: usize) -> Self {
        if let Some(output) = &mut self.output {
            output.tool_limits.push((tool.into(), bytes));
        }
        self
    }

    /// Build the runtime. Recovery runs before the worker claims any task,
    /// so restart reconciliation always precedes new modifying work.
    ///
    /// A `default_provider` is required unless the model loop can never be
    /// the execution owner of an admitted turn (ADR 0026):
    ///
    /// * with a [`Self::native_lane`], the dispatch wrapper owns every turn
    ///   whose frozen target names a vendor runtime; a turn that carries no
    ///   native target reaches a [`RefusingExecutor`] behind the fallback
    ///   and settles failed with `no_model_provider`, so a providerless
    ///   runtime can never be billed for a model loop it does not have;
    /// * otherwise, every turn kind the builder would serve — the chat turn
    ///   kind and each [`Self::turn_kinds`] entry — must have a consumer
    ///   [`Self::executor`], which then owns those turns outright.
    ///
    /// With neither, the missing provider is refused by name.
    pub async fn build(self) -> Result<AgentRuntime, RuntimeError> {
        let Self {
            store,
            clock: runtime,
            default_provider,
            additional_providers,
            extra_contributors,
            external_tools,
            content_authority,
            profiles,
            subagent_max_concurrency,
            plan,
            default_provider_id,
            default_model,
            advisor_sink,
            lease_ttl_ms,
            verification,
            turn_kinds,
            extra_executors,
            native_lane,
            memory,
            memory_sweep,
            embedder,
            reranker,
            memory_extractor,
            output,
        } = self;
        let memory = match memory {
            Some((repository, index)) => {
                let mut service = MemoryService::new(repository, index);
                if let Some(embedder) = embedder {
                    service = service.with_embedder(embedder);
                }
                if let Some(reranker) = reranker {
                    service = service.with_reranker(reranker);
                }
                Some(Arc::new(service))
            }
            None if embedder.is_some() || reranker.is_some() => {
                return Err(RuntimeError::invalid(
                    "`embedder` and `reranker` need a memory service: call `memory(repository, index)`",
                ));
            }
            None => None,
        };
        // The output store: the store handle learns where its files live
        // BEFORE it is cloned into the host, so deleting a chat removes them.
        let store = match &output {
            Some(config) => store.with_output_root(&config.root),
            None => store,
        };
        let mut output_gate = None;
        let mut output_lane = None;
        if let Some(config) = &output {
            let floor = agentkit_core::MIN_OUTPUT_INLINE_LIMIT;
            let too_small = std::iter::once(config.inline_limit)
                .chain(config.tool_limits.iter().map(|(_, bytes)| *bytes))
                .any(|bytes| bytes < floor);
            if too_small {
                return Err(RuntimeError::invalid(format!(
                    "output store: inline limits must be at least {floor} bytes"
                )));
            }
        }
        let mut output_sweeper = None;
        if let Some(config) = output {
            let adapter = agentkit_store::SqliteOutputStore::new(store.clone(), runtime.clone())
                .map_err(|error| RuntimeError::invalid(format!("output store: {error}")))?;
            // Retention sweep: outputs whose 30 days ended. A failed sweep
            // leaves the rows for the next start and never blocks the build.
            let _ = adapter.purge_expired().await;
            output_sweeper = Some(spawn_sweeper(adapter.clone()));
            let shared = Arc::new(adapter);
            let mut gate = agentkit_core::OutputGate::new(shared.clone())
                .with_inline_limit(config.inline_limit);
            for (tool, bytes) in config.tool_limits {
                gate = gate.with_tool_limit(tool, bytes);
            }
            output_gate = Some(Arc::new(gate));
            output_lane = Some(Arc::new(OutputLaneContributor::new(shared.clone(), shared))
                as Arc<dyn ToolContributor>);
        }
        let memory_sweeper = match (&memory, memory_sweep) {
            (Some(service), true) => {
                // Retention sweep: retracted records past their 30 days. A
                // failed sweep leaves them for the next start.
                let _ = service.sweep_retracted(Timestamp(runtime.now_ms())).await;
                Some(spawn_memory_sweeper(service.clone(), runtime.clone()))
            }
            _ => None,
        };
        let memory_indexer = match &memory {
            Some(service) if service.embedder().is_some() => Some(spawn_memory_indexer(
                Arc::downgrade(service),
                runtime.clone(),
            )),
            _ => None,
        };
        // The memory tools, the forget approval and the core block (roadmap
        // 3.3) exist exactly when the runtime has a memory service; the
        // usage flush writes what search and get noted, off the request path.
        let memory_lane = memory
            .as_ref()
            .map(|service| MemoryLane::new(service.clone(), store.clone(), runtime.clone()));
        // Auto-capture (roadmap 3.4): needs the service and a background
        // provider; the executor serves one task kind and the observer queues
        // it when an owner turn settles.
        let memory_capture = match (&memory, memory_extractor) {
            (Some(service), Some((provider, model))) => Some(MemoryCaptureLane::new(
                service.clone(),
                store.clone(),
                runtime.clone(),
                provider,
                model,
            )),
            (None, Some(_)) => {
                return Err(RuntimeError::invalid(
                    "`memory_extractor_provider` needs a memory service: call `memory(repository, index)`",
                ));
            }
            _ => None,
        };
        let memory_flusher = memory
            .as_ref()
            .map(|service| spawn_usage_flusher(service.clone()));
        let has_model_loop = default_provider.is_some();
        let has_native_lane = native_lane.is_some();
        // Every kind the builder itself serves with the turn executor, in a
        // deterministic order: the chat turn kind, then `turn_kinds`.
        let mut builder_kinds: Vec<String> = Vec::new();
        for kind in
            std::iter::once(CHAT_TURN_TASK_KIND.to_owned()).chain(turn_kinds.iter().cloned())
        {
            if !builder_kinds.contains(&kind) {
                builder_kinds.push(kind);
            }
        }
        // Kinds whose builder-owned executor is a refusal stand-in, and may
        // therefore be taken over by a consumer executor (below).
        let mut takeover_allowed: HashSet<String> = if has_model_loop || has_native_lane {
            HashSet::new()
        } else {
            builder_kinds.iter().cloned().collect()
        };
        if !has_model_loop && !has_native_lane {
            let unowned: Vec<&str> = builder_kinds
                .iter()
                .map(String::as_str)
                .filter(|kind| {
                    !extra_executors
                        .iter()
                        .any(|(registered, _)| registered.as_str() == *kind)
                })
                .collect();
            if !unowned.is_empty() {
                return Err(RuntimeError::invalid(format!(
                    "a default provider is required: no model loop is configured and no executor \
                     is registered for {}; supply `default_provider`, `native_lane`, or \
                     `executor(kind, ..)` for every admitted turn kind",
                    unowned.join(", ")
                )));
            }
        }
        let authority = Arc::new(BindingAuthority::new());
        let coding_authority = Arc::clone(&authority) as Arc<dyn CodingPermissionAuthority>;
        let lane = Arc::new(CodingLane::new(
            Arc::clone(&coding_authority),
            store.clone(),
        ));
        // The three late-bound lanes: the executor holds their adapters
        // from construction; the configuration calls below fill them.
        // Unconfigured lanes contribute nothing, judge nothing, and hook
        // nothing — existing services behave exactly as before.
        let subagent_lane = Arc::new(SubagentLane::new(subagent_max_concurrency));
        let advisor_lane = Arc::new(AdvisorLane::new());
        let lsp_lane = Arc::new(LspLane::new(store.clone()));
        let lanes_store = store.clone();
        let external_guard = external_tools
            .map(|policy| JsToolGuard::new(coding_authority, policy.configured_mode, policy.plan));
        let mut routable: Vec<Arc<dyn ChatProvider>> =
            default_provider.clone().into_iter().collect();
        routable.extend(additional_providers);
        // Immutable for the service lifetime: a durable payload's providerId
        // must resolve identically before and after a restart.
        let registry = Arc::new(ProviderRegistry::new(routable));
        let native_provider_factory = Arc::new(
            NativeProviderFactory::default()
                .with_static_provider_ids(registry.ids().map(str::to_owned).collect()),
        );
        let profiles = Arc::new(ProfileRegistry::new(&profiles).map_err(|error| {
            RuntimeError::Sessions(agentkit_sessions::SessionsError::from_faultless(error))
        })?);
        let mut tool_contributors = extra_contributors;
        tool_contributors.extend(output_lane);
        tool_contributors.extend(memory_lane.as_ref().map(|lane| lane.contributor.clone()));
        tool_contributors.extend([
            // The Rust coding toolpack, once registered, joins the SAME
            // lane: same snapshot, same durable execution, same receipts.
            Arc::new(CodingLaneContributor::new(Arc::clone(&lane))) as Arc<dyn ToolContributor>,
            // Delegation, advisory, and code-intelligence namespaces:
            // late-bound lanes; each offers nothing until its
            // configuration call arrives.
            Arc::new(SubagentLaneContributor::new(Arc::clone(&subagent_lane)))
                as Arc<dyn ToolContributor>,
            Arc::new(AdvisorLaneContributor::new(Arc::clone(&advisor_lane)))
                as Arc<dyn ToolContributor>,
            Arc::new(LspLaneContributor::new(Arc::clone(&lsp_lane))) as Arc<dyn ToolContributor>,
        ]);
        let mut tool_guards: Vec<Arc<dyn ToolGuard>> = Vec::new();
        if let Some(guard) = external_guard {
            tool_guards.push(Arc::new(guard));
        }
        tool_guards.extend(memory_lane.as_ref().map(|lane| lane.guard.clone()));
        tool_guards.extend([
            // Both coding-namespace guards of the crate's own chain: the
            // permission matrix and the high-risk fresh-approval guard.
            // Each judges only the `coding` namespace; the JS namespace
            // stays on the JS guard — guards compose with AND.
            Arc::new(CodingLaneGuard::new(Arc::clone(&lane))) as Arc<dyn ToolGuard>,
            Arc::new(agentkit_toolpacks_coding::HighRiskCommandGuard) as Arc<dyn ToolGuard>,
            // Subagent namespace ceilings + workspace write coordination;
            // judges only subagent tools, and read-only-role children only
            // through the crate's own guard. Passes everything while
            // unconfigured.
            Arc::new(SubagentLaneGuard::new(Arc::clone(&subagent_lane))) as Arc<dyn ToolGuard>,
        ]);
        // The model loop, when there is a provider to run it. Without one
        // the composition root has no model loop at all: the kinds the
        // builder would serve are held by a typed refusal, so a turn that
        // needs a model settles failed instead of running or panicking.
        let model_loop = default_provider.as_ref().map(|provider| {
            AgentExecutor::new(Arc::clone(provider), Arc::new(ToolRegistry::new())).with_options(
                AgentExecutorOptions {
                    providers: Some(Arc::clone(&registry)),
                    provider_factory: Some(native_provider_factory.clone()),
                    content_authority: content_authority.clone(),
                    tool_contributors,
                    tool_guards,
                    // Exact-call remembering for native shell approvals:
                    // the crate's fingerprint-v4 policy over the registered
                    // lane's workspace. Unregistered resolves to nothing,
                    // so every approvable call keeps asking.
                    standing: Some(LaneWorkspaceResolver::standing_policy(Arc::clone(&lane))
                        as Arc<dyn agentkit_governance::StandingApprovalPolicy>),
                    // Sessions whose descriptor carries a workspace get the
                    // AGENTS.md/CLAUDE.md system block exactly as the
                    // TypeScript authority injects it; descriptor-less
                    // sessions stay untouched. Refusals (symlink, escape)
                    // degrade to a run warning, never a lost turn.
                    project_instructions: Some(Arc::new(agentkit_chat::FileProjectInstructions)),
                    // Delegation instructions enter every main-agent run
                    // while subagents are configured+enabled; the watchdog
                    // observes every run loop. Both are observational:
                    // unconfigured they add nothing and never fail a turn.
                    delegation_guidance: Some(
                        Arc::clone(&subagent_lane) as Arc<dyn agentkit_chat::DelegationGuidance>
                    ),
                    run_hooks: Some(Arc::clone(&advisor_lane) as Arc<dyn RunHooks>),
                    // Token budgets of subagent children, checked before
                    // every provider request; non-children pass through.
                    usage: Some(
                        Arc::clone(&subagent_lane) as Arc<dyn agentkit_chat::UsageAuthorizer>
                    ),
                    output: output_gate,
                    core_memory: memory_lane.as_ref().map(|lane| lane.core.clone()),
                    core_memory_refreeze: memory_lane.as_ref().map(|lane| lane.refreeze.clone()),
                    ..Default::default()
                },
            )
        });
        let chat: Arc<dyn TaskExecutor<agentkit_store::SqliteStore>> = match model_loop {
            Some(executor) => Arc::new(executor),
            None => Arc::new(RefusingExecutor::no_model_provider()),
        };
        // One execution owner per admitted turn (ADR 0025 decision 1):
        // with a native lane the turn executor is the dispatch wrapper, so
        // a native-target payload runs the durable native path and
        // everything else keeps the model loop (AGENTKIT-107 §2) — or, with
        // no provider behind it, the refusal that says so.
        let turn: Arc<dyn TaskExecutor<agentkit_store::SqliteStore>> = match native_lane {
            Some(lane) => Arc::new(NativeDispatchExecutor {
                native: Arc::new(NativeAgentExecutor::new(
                    store.clone(),
                    lane.backend,
                    lane.supervisor,
                    lane.locator,
                    lane.broker_factory,
                    lane.options,
                )),
                fallback: Arc::clone(&chat),
            }),
            None => chat,
        };
        let mut executors: HashMap<String, Arc<dyn TaskExecutor<agentkit_store::SqliteStore>>> =
            HashMap::from([(CHAT_TURN_TASK_KIND.to_owned(), Arc::clone(&turn))]);
        for kind in turn_kinds {
            executors.insert(kind, Arc::clone(&turn));
        }
        if let Some(capture) = &memory_capture {
            executors.insert(
                MEMORY_EXTRACT_TASK_KIND.to_owned(),
                Arc::clone(&capture.executor),
            );
        }
        for (kind, executor) in extra_executors {
            // One execution owner per kind: a collision is a build-time
            // refusal, never a silent shadow — except where the builder's
            // own executor for that kind was only the refusal stand-in,
            // which the consumer is entitled to replace with a real owner.
            if executors.insert(kind.clone(), executor).is_some() && !takeover_allowed.remove(&kind)
            {
                return Err(RuntimeError::invalid(format!(
                    "executor kind registered twice: {kind}"
                )));
            }
        }
        let host = AgentHost::new(
            store.clone(),
            executors,
            runtime.clone(),
            HostOptions {
                // Delegation safety: a durable parent turn parks in
                // `WaitingChild` while `subagent_await` waits and holds no
                // slot, but a turn that cannot park (or a parent between
                // awaits) still runs, so the pool hosts the root sessions,
                // their parents, AND in-flight children — with the default
                // subagent limits (8 children active, depth 2, 4 per
                // parent) a pool sized only for the parents could starve
                // children. Idle workers park on the wake queue;
                // overprovisioning is free. The child term is the START-TIME subagent
                // `maxConcurrency`; `configureSubagents` clamps later raises
                // to it, because the pool is fixed once the worker starts.
                lease_ttl_ms,
                verification,
                concurrency: BASE_WORKERS + usize::try_from(subagent_max_concurrency).unwrap_or(0),
                // Orphan reaping on parent settlement (subagents) and
                // turn-failure tracking (watchdog). Failures never unwind
                // the settlement; the startup sweep is the same computation.
                settled_observers: [
                    Arc::clone(&subagent_lane) as Arc<dyn agentkit_engine::TaskSettledObserver>,
                    Arc::clone(&advisor_lane) as Arc<dyn agentkit_engine::TaskSettledObserver>,
                ]
                .into_iter()
                .chain(memory_capture.as_ref().map(|lane| lane.observer.clone()))
                .collect(),
                ..Default::default()
            },
        )?;
        let mut service = AgentSessionService::new(AgentSessionServiceDeps {
            store,
            runtime: runtime.clone(),
            plan_approval: Some(Arc::new(BindingPlanPolicy::new(plan))),
            wake: Some(host.wake_handle()),
        })
        .with_profiles(profiles.clone())
        .with_defaults(default_provider_id, default_model);
        if let Some(authority) = &content_authority {
            service = service.with_content_authority(authority.clone());
        }
        let service = service
            .with_provider_factory(native_provider_factory.clone())
            .with_gate(host.admission_gate());
        host.set_session_flush_planner(Arc::new(service.clone()))
            .await;
        // Bound before the worker exists: a run can never query an unbound
        // authority and fall back to the configured mode by accident.
        authority.bind(service.clone());
        // Fill the late-bound lanes. Subagent coordination needs the
        // session service (child turns are ordinary durable turns); the
        // advisor needs the provider, store, clock, tool source, event
        // bridge, and session service. Both are one-shot; configuration
        // (`configureSubagents`/`configureAdvisor`) is hot afterwards.
        let mut subagents = agentkit_agents::SubagentService::new(
            lanes_store.clone(),
            service.clone(),
            runtime.clone(),
            agentkit_agents::SubagentConfig::default(),
        )
        .with_routable_providers(registry.ids().map(str::to_owned).collect());
        // Without a memory service a brief's `memory` references are refused.
        if let Some(memory) = &memory {
            subagents = subagents.with_memory_refs(Arc::new(
                crate::memory_refs::MemoryRefAccess::new(Arc::clone(memory)),
            ));
        }
        subagent_lane.bind(subagents)?;
        // The advisor advises with the default provider. With no provider
        // there is nothing to advise with, so the lane stays unconfigured —
        // it contributes nothing, judges nothing, and hooks nothing, exactly
        // as before any configuration call.
        if let Some(advisor_provider) = default_provider {
            advisor_lane.bind(
                advisor_provider,
                Arc::clone(&registry),
                lanes_store,
                runtime.clone(),
                Arc::new(LaneAdvisorTools {
                    coding: Arc::clone(&lane),
                    lsp: Arc::clone(&lsp_lane),
                }),
                advisor_sink,
                service.clone(),
            )?;
        }
        // Crash-reconcile interrupted turns BEFORE the worker can claim work.
        host.recover().await?;
        // This process's previous life may have died mid-child: children
        // whose parent already settled are reaped before any new turn can
        // spawn under the same identities.
        subagent_lane.reap_orphans().await?;
        let worker = host.start_worker();
        Ok(AgentRuntime {
            host,
            service,
            content_authority,
            native_provider_factory,
            profiles,
            registry,
            coding: lane,
            subagent: subagent_lane,
            advisor: advisor_lane,
            lsp: lsp_lane,
            memory,
            worker: Some(worker),
            _output_sweeper: output_sweeper,
            _memory_sweeper: memory_sweeper,
            _memory_indexer: memory_indexer,
            _memory_flusher: memory_flusher,
        })
    }
}

/// The turn executor of a runtime with no model loop: it settles every turn
/// it is handed failed with a fixed, deterministic code, and never runs,
/// bills, or panics. A providerless composition root registers it for the
/// turn kinds it still owns — behind the native dispatch wrapper's fallback,
/// or as the takeover target a consumer replaces with its own executor — so
/// a turn admitted under a kind the runtime cannot execute produces a durable
/// refusal instead of a surprise.
pub struct RefusingExecutor {
    code: String,
    message: String,
}

impl RefusingExecutor {
    /// The code of the refusal the composition root installs for a turn that
    /// would need a model loop the runtime does not have.
    pub const NO_MODEL_PROVIDER: &'static str = "no_model_provider";

    /// The stand-in the builder registers when no provider was configured.
    pub fn no_model_provider() -> Self {
        Self::new(
            Self::NO_MODEL_PROVIDER,
            "this runtime has no model loop: no default provider was configured",
        )
    }

    /// A refusal with a consumer-chosen code and message. The code leads the
    /// message so the durable task record names it whatever the runner does
    /// with the structured failure.
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        let code = code.into();
        Self {
            message: format!("{code}: {}", message.into()),
            code,
        }
    }
}

#[async_trait]
impl TaskExecutor<agentkit_store::SqliteStore> for RefusingExecutor {
    async fn execute(
        &self,
        _context: agentkit_engine::ExecutionContext<agentkit_store::SqliteStore>,
    ) -> agentkit_engine::EngineResult<(), agentkit_store::StoreError> {
        // Nothing ran, so nothing can have landed: replay is provably safe,
        // and the refusal is terminal because no retry can supply a provider.
        Err(agentkit_engine::EngineError::Failure(ExecutionFailure {
            message: self.message.clone(),
            code: Some(self.code.clone()),
            retryable: Some(false),
            http_status: None,
            retry_safety: RetrySafety::SafeToRetry,
        }))
    }
}
