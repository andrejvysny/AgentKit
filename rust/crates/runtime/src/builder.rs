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
use agentkit_chat::{ChatExecutor, ChatExecutorOptions, ProviderRegistry};
use agentkit_core::permission::CodingMode;
use agentkit_core::{ChatProvider, RunHooks, ToolRegistry};
use agentkit_engine::{
    ExecutionFailure, RetrySafety, SharedRuntime, SystemRuntime, TaskExecutor, VerificationHook,
};
use agentkit_governance::{CodingPermissionAuthority, ToolContributor, ToolGuard};
use agentkit_native_exec::{NativeAgentExecutor, NativeDispatchExecutor};
use agentkit_providers::NativeProviderFactory;
use agentkit_sessions::{
    AgentHost, AgentSessionService, AgentSessionServiceDeps, CHAT_TURN_TASK_KIND, ContentAuthority,
    HostOptions, ProfileRegistry,
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
use crate::lanes::native::NativeLane;
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
    content_authority: Option<Arc<dyn ContentAuthority>>,
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

    pub fn content_authority(mut self, authority: Arc<dyn ContentAuthority>) -> Self {
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
        } = self;
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
        let profiles = Arc::new(ProfileRegistry::new(&profiles)?);
        let mut tool_contributors = extra_contributors;
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
            ChatExecutor::new(Arc::clone(provider), Arc::new(ToolRegistry::new())).with_options(
                ChatExecutorOptions {
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
                // Delegation safety: a parent turn holds its worker slot
                // while `subagent_await` polls, so the pool must host the
                // root sessions, their delegating parents, AND in-flight
                // children — with the default subagent limits (8 children
                // active, depth 2, 4 per parent) a pool sized only for the
                // parents starves every child until the await times out.
                // Idle workers park on the wake queue; overprovisioning is
                // free. The child term is the START-TIME subagent
                // `maxConcurrency`; `configureSubagents` clamps later raises
                // to it, because the pool is fixed once the worker starts.
                lease_ttl_ms,
                verification,
                concurrency: BASE_WORKERS + usize::try_from(subagent_max_concurrency).unwrap_or(0),
                // Orphan reaping on parent settlement (subagents) and
                // turn-failure tracking (watchdog). Failures never unwind
                // the settlement; the startup sweep is the same computation.
                settled_observers: vec![
                    Arc::clone(&subagent_lane) as Arc<dyn agentkit_engine::TaskSettledObserver>,
                    Arc::clone(&advisor_lane) as Arc<dyn agentkit_engine::TaskSettledObserver>,
                ],
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
        subagent_lane.bind(
            agentkit_agents::SubagentService::new(
                lanes_store.clone(),
                service.clone(),
                runtime.clone(),
                agentkit_agents::SubagentConfig::default(),
            )
            .with_routable_providers(registry.ids().map(str::to_owned).collect()),
        )?;
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
            worker: Some(worker),
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
