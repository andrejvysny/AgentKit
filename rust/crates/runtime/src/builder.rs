//! Runtime composition. The order below is load-bearing and mirrors what the
//! binding did inline: lanes and authority first, then the provider and
//! profile registries, the executor, the host, the session service, the
//! late binding of the authority and lanes, recovery, and only then the
//! worker.

use std::collections::HashMap;
use std::sync::Arc;

use agentkit_core::permission::CodingMode;
use agentkit_core::{ChatProvider, RunHooks, ToolRegistry};
use agentkit_host::{
    AdvisorEventSink, AgentHost, AgentSessionService, AgentSessionServiceDeps, CHAT_TURN_TASK_KIND,
    ChatExecutor, ChatExecutorOptions, CodingPermissionAuthority, ContentAuthority, HostOptions,
    ProfileRegistry, ProviderRegistry, SharedRuntime, SystemRuntime, TaskExecutor, ToolContributor,
    ToolGuard, VerificationHook,
};
use agentkit_providers::NativeProviderFactory;
use serde_json::Value;

use crate::authority::{BindingAuthority, BindingPlanPolicy, JsToolGuard, PlanConfiguration};
use crate::error::RuntimeError;
use crate::lanes::advisor::{AdvisorLane, AdvisorLaneContributor, LaneAdvisorTools};
use crate::lanes::coding::{
    CodingLane, CodingLaneContributor, CodingLaneGuard, LaneWorkspaceResolver,
};
use crate::lanes::lsp::{LspLane, LspLaneContributor};
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
    verification: Option<Arc<dyn VerificationHook>>,
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
            subagent_max_concurrency: agentkit_host::SubagentConfig::default()
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
        }
    }

    pub fn clock(mut self, clock: SharedRuntime) -> Self {
        self.clock = clock;
        self
    }

    /// The provider that serves turns when a session pins none (required).
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
    pub fn verification(mut self, hook: Arc<dyn VerificationHook>) -> Self {
        self.verification = Some(hook);
        self
    }

    /// Build the runtime. Recovery runs before the worker claims any task,
    /// so restart reconciliation always precedes new modifying work.
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
        } = self;
        let provider = default_provider
            .ok_or_else(|| RuntimeError::invalid("a default provider is required"))?;
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
        let mut routable = vec![Arc::clone(&provider)];
        routable.extend(additional_providers);
        // Immutable for the service lifetime: a durable payload's providerId
        // must resolve identically before and after a restart.
        let registry = Arc::new(ProviderRegistry::new(routable));
        let native_provider_factory = Arc::new(
            NativeProviderFactory::default()
                .with_static_provider_ids(registry.ids().map(str::to_owned).collect()),
        );
        let profiles = Arc::new(ProfileRegistry::new(&profiles)?);
        let advisor_provider = Arc::clone(&provider);
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
        let executor: Arc<dyn TaskExecutor> = Arc::new(
            ChatExecutor::new(provider, Arc::new(ToolRegistry::new())).with_options(
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
                        as Arc<dyn agentkit_host::StandingApprovalPolicy>),
                    // Sessions whose descriptor carries a workspace get the
                    // AGENTS.md/CLAUDE.md system block exactly as the
                    // TypeScript authority injects it; descriptor-less
                    // sessions stay untouched. Refusals (symlink, escape)
                    // degrade to a run warning, never a lost turn.
                    project_instructions: Some(Arc::new(agentkit_host::FileProjectInstructions)),
                    // Delegation instructions enter every main-agent run
                    // while subagents are configured+enabled; the watchdog
                    // observes every run loop. Both are observational:
                    // unconfigured they add nothing and never fail a turn.
                    delegation_guidance: Some(
                        Arc::clone(&subagent_lane) as Arc<dyn agentkit_host::DelegationGuidance>
                    ),
                    run_hooks: Some(Arc::clone(&advisor_lane) as Arc<dyn RunHooks>),
                    ..Default::default()
                },
            ),
        );
        let host = AgentHost::new(
            store.clone(),
            HashMap::from([(CHAT_TURN_TASK_KIND.to_owned(), executor)]),
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
                    Arc::clone(&subagent_lane) as Arc<dyn agentkit_host::TaskSettledObserver>,
                    Arc::clone(&advisor_lane) as Arc<dyn agentkit_host::TaskSettledObserver>,
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
            agentkit_host::SubagentService::new(
                lanes_store.clone(),
                service.clone(),
                runtime.clone(),
                agentkit_host::SubagentConfig::default(),
            )
            .with_routable_providers(registry.ids().map(str::to_owned).collect()),
        )?;
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
