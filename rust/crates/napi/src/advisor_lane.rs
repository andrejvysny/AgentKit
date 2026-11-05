//! The advisor/watchdog lane: late-bound advisory review over the same
//! engine. Unconfigured means "no advisor": no tools contributed, no hooks
//! observed, no settled observations.

use std::sync::{Arc, Mutex, PoisonError};

use agentkit_core::{ChatProvider, RunHooks};
use agentkit_host::{
    AdvisorConfig, AdvisorContributor, AdvisorEngine, AdvisorEventSink, AdvisorReport,
    AdvisorToolSource, AdvisorTools, AgentSessionService, ContributedTool, HostResult,
    ProviderRegistry, SharedRuntime, TaskSettledObserver, ToolContributionContext, ToolContributor,
    Watchdog, WatchdogRunHooks,
};
use agentkit_store::TaskRecord;
use std::sync::Mutex as StdMutex;

use crate::errors;

/// The bridge forwards `advisor.finding` escalations to the JS host as
/// ordinary service events (same channel, `kind` discriminator).
pub(crate) struct BridgeAdvisorSink {
    bridge: Arc<crate::jsbridge::JsBridge>,
}

impl BridgeAdvisorSink {
    pub(crate) fn new(bridge: Arc<crate::jsbridge::JsBridge>) -> Self {
        Self { bridge }
    }
}

impl AdvisorEventSink for BridgeAdvisorSink {
    fn emit(&self, event: serde_json::Value) {
        let status = self.bridge.deliver_message(event.to_string());
        if status != napi::Status::Ok {
            // An undeliverable ESCALATION must at least leave a trace: a
            // critical finding vanishing into a full/closing JS queue is
            // exactly the failure the event channel exists to prevent.
            eprintln!("advisor escalation dropped ({status:?}): {}", event);
        }
    }
}

/// The advisor's read-only tools: the coding pack's read side plus every
/// LSP tool. Anything effect-bearing is excluded twice over — by this
/// filter and by the advisor run's missing durable-execution authority.
pub(crate) struct LaneAdvisorTools {
    pub(crate) coding: Arc<crate::coding_lane::CodingLane>,
    pub(crate) lsp: Arc<crate::lsp_lane::LspLane>,
}

#[async_trait::async_trait]
impl AdvisorToolSource for LaneAdvisorTools {
    /// Bound to the REVIEWED session: a review of a session that is not
    /// bound to the registered workspace must not read that workspace (its
    /// findings would carry the content back to that session).
    async fn read_tools(&self, chat_id: &str) -> Vec<Arc<dyn agentkit_core::Tool>> {
        let mut tools = self.coding.contribute_read_tools(Some(chat_id)).await;
        tools.extend(self.lsp.read_tools_for(Some(chat_id)).await);
        tools
    }
}

struct AdvisorState {
    engine: AdvisorEngine,
    watchdog: Watchdog,
    hooks: Arc<WatchdogRunHooks>,
    tools: AdvisorTools,
    store: agentkit_store::SqliteStore,
}

/// The one mutable piece of the advisor lane.
pub(crate) struct AdvisorLane {
    state: StdMutex<Option<Arc<AdvisorState>>>,
    config: Arc<Mutex<AdvisorConfig>>,
    /// The default provider id and the routable registry, set at bind so
    /// `configureAdvisor` can refuse an unroutable pin up front.
    routing: StdMutex<Option<(String, Arc<ProviderRegistry>)>>,
}

impl AdvisorLane {
    pub(crate) fn new() -> Self {
        Self {
            state: StdMutex::new(None),
            config: Arc::new(Mutex::new(AdvisorConfig::default())),
            routing: StdMutex::new(None),
        }
    }

    /// Configure-time form of the engine's review-time provider checks:
    /// an unknown pin is `invalid_input`, and a pin onto a non-default
    /// provider needs its own model (the session's model names the wrong
    /// provider's catalogue).
    pub(crate) fn validate(&self, config: &AdvisorConfig) -> Result<(), napi::Error> {
        let routing = self.routing.lock().unwrap_or_else(PoisonError::into_inner);
        let Some((default_id, registry)) = routing.as_ref() else {
            return Ok(());
        };
        for (key, pinned, model_key, model) in [
            ("providerId", &config.provider_id, "model", &config.model),
            (
                "watchdogProviderId",
                &config.watchdog_provider_id,
                "watchdogModel",
                &config.watchdog_model,
            ),
        ] {
            let Some(id) = pinned.as_deref().filter(|id| !id.is_empty()) else {
                continue;
            };
            if registry.get(id).is_none() {
                return Err(errors::invalid(format!(
                    "{key} \"{id}\" is not a configured provider"
                )));
            }
            if id != default_id && model.as_deref().is_none_or(str::is_empty) {
                return Err(errors::invalid(format!(
                    "{key} \"{id}\" differs from the default provider; {model_key} is required"
                )));
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn bind(
        &self,
        provider: Arc<dyn ChatProvider>,
        providers: Arc<ProviderRegistry>,
        store: agentkit_store::SqliteStore,
        runtime: SharedRuntime,
        tools: Arc<dyn AdvisorToolSource>,
        events: Option<Arc<dyn AdvisorEventSink>>,
        session: AgentSessionService,
    ) -> Result<(), napi::Error> {
        let mut slot = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if slot.is_some() {
            return Err(errors::invalid(
                "the advisor is already configured on this service",
            ));
        }
        *self.routing.lock().unwrap_or_else(PoisonError::into_inner) =
            Some((provider.id().to_owned(), Arc::clone(&providers)));
        let config = self
            .config
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .effective();
        // The advisor surface and the watchdog share the config cell so
        // `configureAdvisor` is one write with both effects: the engine
        // ADOPTS the lane cell (no private copy that could lag the lane
        // during a hot update).
        let engine = AdvisorEngine::new(
            provider,
            store.clone(),
            Arc::new(CoreRuntimeAdapter(runtime.clone())),
            config,
            tools,
            events,
            session,
        )
        .with_shared_config(Arc::clone(&self.config))
        .with_providers(providers);
        let watchdog = Watchdog::new(engine.clone(), Arc::clone(&self.config), runtime.clone());
        let hooks = Arc::new(WatchdogRunHooks::new(watchdog.clone(), store.clone()));
        let advisor_tools = AdvisorTools::new(engine.clone(), Arc::clone(&self.config));
        *slot = Some(Arc::new(AdvisorState {
            engine,
            watchdog,
            hooks,
            tools: advisor_tools,
            store,
        }));
        Ok(())
    }

    pub(crate) fn configure(&self, config: AdvisorConfig) {
        let config = config.effective();
        *self.config.lock().unwrap_or_else(PoisonError::into_inner) = config.clone();
        if let Some(state) = self.state() {
            // The engine shares this cell; only the permit pool needs a
            // separate resize step.
            state.engine.sync_review_pool(&config);
        }
    }

    pub(crate) fn view(&self) -> serde_json::Value {
        let config = self
            .config
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        serde_json::to_value(&config).unwrap_or_else(|_| serde_json::json!({}))
    }

    fn state(&self) -> Option<Arc<AdvisorState>> {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub(crate) async fn review(&self, chat_id: &str, focus: String) -> HostResult<AdvisorReport> {
        let state = self.state().ok_or_else(|| {
            agentkit_host::HostError::Execution(crate::errors::ADVISOR_NOT_CONFIGURED.into())
        })?;
        agentkit_host::explicit_review(
            &state.engine,
            chat_id,
            focus,
            agentkit_core::CancellationToken::new(),
        )
        .await
    }

    pub(crate) async fn findings(
        &self,
        chat_id: &str,
        limit: i64,
    ) -> HostResult<Vec<agentkit_store::AdvisorFindingRecord>> {
        let state = self.state().ok_or_else(|| {
            agentkit_host::HostError::Execution(crate::errors::ADVISOR_NOT_CONFIGURED.into())
        })?;
        let records = state.store.list_advisor_findings(chat_id, limit).await?;
        Ok(records)
    }
}

/// Adapts the host's shared runtime to the core runtime trait for the
/// advisor engine's clock.
struct CoreRuntimeAdapter(SharedRuntime);
impl agentkit_core::Runtime for CoreRuntimeAdapter {
    fn timestamp(&self) -> String {
        agentkit_store::ConversationTimestamp::from_epoch_millis(self.0.now_ms())
            .expect("host clock must fit JavaScript Date range")
            .wire_text()
            .expect("validated host clock")
            .into_owned()
    }
    fn id(&self, prefix: &str) -> String {
        format!("{prefix}{}", self.0.id())
    }
    fn now_ms(&self) -> u64 {
        self.0.now_ms().max(0) as u64
    }
}

/// Contributor adapter: `advisor_review` while the advisor is enabled.
pub(crate) struct AdvisorLaneContributor {
    lane: Arc<AdvisorLane>,
}

impl AdvisorLaneContributor {
    pub(crate) fn new(lane: Arc<AdvisorLane>) -> Self {
        Self { lane }
    }
}

#[async_trait::async_trait]
impl ToolContributor for AdvisorLaneContributor {
    fn namespace(&self) -> &str {
        "advisor"
    }
    async fn contribute(
        &self,
        context: &ToolContributionContext,
    ) -> HostResult<Vec<ContributedTool>> {
        let Some(state) = self.lane.state() else {
            return Ok(vec![]);
        };
        AdvisorContributor::new(state.tools.clone())
            .contribute(context)
            .await
    }
}

/// Run-hooks adapter: watchdog observation of live run loops.
#[async_trait::async_trait]
impl RunHooks for AdvisorLane {
    fn indexes(&self, name: &str) -> Vec<usize> {
        match self.state() {
            Some(state) => state.hooks.indexes(name),
            None => vec![],
        }
    }
    async fn call(
        &self,
        name: &str,
        index: usize,
        context: serde_json::Value,
        cancellation: &agentkit_core::CancellationToken,
    ) -> Result<serde_json::Value, agentkit_core::CoreError> {
        match self.state() {
            Some(state) => state.hooks.call(name, index, context, cancellation).await,
            None => Ok(serde_json::Value::Null),
        }
    }
}

/// Settled-observer adapter: watchdog turn-failure tracking.
#[async_trait::async_trait]
impl TaskSettledObserver for AdvisorLane {
    async fn observe_settled(&self, task: &TaskRecord) -> HostResult<()> {
        match self.state() {
            Some(state) => {
                let result = state.watchdog.observe_settled(task).await;
                // The hooks' run→chat identity cache is the watchdog's
                // second per-run map; release it with the run itself.
                state.hooks.forget_run(&task.task_id);
                result
            }
            None => Ok(()),
        }
    }
}
