//! The session-service binding: construction, the durable-input surface,
//! event subscription, and shutdown.
//!
//! Inputs arrive as JSON text (the JS wrapper owns camelCase mapping); every
//! response crosses back as JSON text so the portable contract — not a
//! parallel Rust-only schema — defines the wire shape. Shutdown order is
//! explicit: reject outstanding callbacks, stop the worker, drain queued
//! store writes, release the store clone. A service dropped WITHOUT `close()`
//! (GC, or Node tearing the environment down) still gets a best-effort,
//! non-blocking version of it: the bridge starts closing and the worker and
//! listeners are stopped so no orphan task keeps running against a dead
//! host. Durable work is unaffected - it recovers on the next boot.

use std::ops::DerefMut;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use agentkit_core::permission::CodingMode;
use agentkit_engine::RunUpdate;
use agentkit_runtime::{AgentRuntime, CodingLane, ExternalToolsPolicy, PlanConfiguration};
use agentkit_sessions::{HostResult, RunHandle};
use napi::bindgen_prelude::*;
use napi::threadsafe_function::ThreadsafeFunction;
use serde_json::{Value, json};
use tokio::sync::{Mutex, Notify, mpsc};
use tokio_util::sync::CancellationToken;

use crate::advisor_sink::BridgeAdvisorSink;
use crate::errors;
use crate::jsbridge::JsBridge;
use crate::provider::JsProvider;
use crate::tool::JsToolContributor;

#[path = "http_server.rs"]
mod http_server;
#[path = "service_management.rs"]
mod management;
#[path = "mcp_server.rs"]
mod mcp_server;
#[path = "service_receipts.rs"]
mod receipts;

const MAX_POLL_BATCH: i32 = 64;
const DEFAULT_POLL_WAIT_MS: u64 = 25;
const DEFAULT_CLOSE_TIMEOUT_MS: u64 = 5_000;
const MAX_CLOSE_TIMEOUT_MS: u64 = 300_000;
const MAX_SUBAGENT_POOL: i64 = 64;

struct Core {
    runtime: AgentRuntime,
    provider_ports: Arc<crate::provider_connection::ports::BridgeProviderPorts>,
    provider_memory_secrets: Arc<agentkit_connections::MemorySecretStore>,
    mcp_config_runtime: Arc<dyn agentkit_mcp::McpConfigRuntime>,
    mcp_config_secrets: Arc<dyn agentkit_mcp::McpConfigSecrets>,
    contributor: Arc<JsToolContributor>,
    subscriptions: Subscriptions,
    http: Option<http_server::HttpServer>,
    mcp: Option<mcp_server::McpServerHandle>,
}

/// Live subscriptions of one service. A std mutex on purpose: pruning runs
/// from synchronous `cancel()` and from `Drop`-adjacent paths and is never
/// held across an await.
type Subscriptions = Arc<std::sync::Mutex<Vec<Arc<Subscription>>>>;

#[napi(js_name = "SessionService")]
pub struct SessionService {
    core: Arc<Mutex<Option<Core>>>,
    bridge: Arc<JsBridge>,
    gate: Arc<CloseGate>,
    close_timeout: Duration,
    uncertain: Arc<AtomicBool>,
    /// The Node environment (raw `napi_env`) that owns this service; 0
    /// until `bindEnv`. Environment cleanup drains only its own services.
    env: Arc<std::sync::atomic::AtomicUsize>,
}

/// One-shot shutdown coordination: concurrent `close` callers await the
/// same completion instead of racing the teardown or returning early.
struct CloseGate {
    state: std::sync::Mutex<GateState>,
    wake: Notify,
}

impl CloseGate {
    fn lock(&self) -> std::sync::MutexGuard<'_, GateState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Resets `busy` (and wakes waiters) however `close()` ends - success,
/// error, panic, or the future being dropped mid-shutdown - so a failed
/// attempt can never wedge every later `close()` behind a stale flag.
struct BusyGuard<'a> {
    gate: &'a CloseGate,
    done: bool,
}

impl Drop for BusyGuard<'_> {
    fn drop(&mut self) {
        {
            let mut state = self.gate.lock();
            state.busy = false;
            state.done = self.done;
        }
        self.gate.wake.notify_waiters();
    }
}

enum GateAction {
    Done,
    Wait,
    Run,
}

struct GateState {
    busy: bool,
    done: bool,
}

#[napi]
impl SessionService {
    /// Build the full native runtime over a native store: host + chat
    /// executor + session service + worker. Recovery runs before the worker
    /// claims any task, so restart reconciliation always precedes new
    /// modifying work. Exposed on the JS side as `NativeStore.createService`.
    ///
    /// `optionsJson` (optional): `{closeTimeoutMs?, jsToolAuthority?:
    /// {configuredMode?, planToolNames?, planGrantMode?}, providerConfig?:
    /// {protocol, providerId, connectionId, model, temperature?,
    /// maxTokens?}, providerAuthKind?: "api-key"|"chatgpt-account",
    /// profiles?: AgentProfile[]}`.
    /// With `profiles`, the host profile registry is the built-ins with those
    /// definitions merged over them (same id overrides, `extends` inherits),
    /// exactly as `ProfileRegistry::new` merges; absent, it is the built-ins
    /// alone. Sessions admit only profiles that registry knows.
    /// With `providerConfig`, provider HTTP/SSE executes in
    /// Rust over a connection registered via `registerProviderConnection`
    /// (keys never appear in `optionsJson`); without it, the JS callback
    /// provider slot serves turns as before. `providerAuthKind` declares
    /// the JS callback lane's parameter capabilities: `"chatgpt-account"`
    /// (an account-backed adapter that rejects sampling/output parameters)
    /// suppresses the implicit output cap and omits undeclared options from
    /// the bridge payload; the default `"api-key"` lane is unchanged. JS write tools
    /// are governed by the Rust coding-pack authority (plan mode blocks
    /// them; ask mode asks per call; only plan-tool approval escalates,
    /// durably) — an unsupported configuration is refused at construction,
    /// never silently ignored.
    pub(crate) async fn build(
        store_handle: agentkit_store::SqliteStore,
        on_event: ThreadsafeFunction<String>,
        options_json: Option<String>,
        mcp_ports: (
            Arc<dyn agentkit_mcp::McpConfigRuntime>,
            Arc<dyn agentkit_mcp::McpConfigSecrets>,
        ),
    ) -> Result<Self> {
        let options = parse_service_options(options_json.as_deref())?;
        let bridge = Arc::new(JsBridge::new(on_event));
        let contributor = Arc::new(JsToolContributor::new(Arc::clone(&bridge)));
        // The same fence the callback bridge enforces: once `close()` has
        // begun, no new provider request may leave the process.
        let fence_bridge = Arc::clone(&bridge);
        let fence: crate::provider_connection::Fence = Arc::new(move || {
            if fence_bridge.is_open() {
                Ok(())
            } else {
                Err(agentkit_core::CoreError::new(
                    "cancelled",
                    "the binding is closing",
                ))
            }
        });
        let provider: Arc<dyn agentkit_core::ChatProvider> = match &options.provider {
            None => Arc::new(
                JsProvider::new(Arc::clone(&bridge))
                    .with_account_backed(options.account_backed_provider),
            ),
            Some(lane) => build_lane_provider(lane, Arc::clone(&fence)).await?,
        };
        let mut additional: Vec<Arc<dyn agentkit_core::ChatProvider>> = Vec::new();
        for lane in &options.additional_providers {
            additional.push(build_lane_provider(lane, Arc::clone(&fence)).await?);
        }
        let provider_ports = Arc::new(crate::provider_connection::ports::BridgeProviderPorts::new(
            bridge.clone(),
        ));
        let provider_memory_secrets = Arc::new(agentkit_connections::MemorySecretStore::default());
        let content_authority: Arc<dyn agentkit_sessions::ContentAuthority> =
            Arc::new(crate::provider::JsContentAuthority(Arc::clone(&bridge)));
        let runtime = AgentRuntime::builder(store_handle)
            .default_provider(provider)
            .additional_providers(additional)
            .extra_contributors(vec![
                Arc::clone(&contributor) as Arc<dyn agentkit_governance::ToolContributor>
            ])
            .external_tools(ExternalToolsPolicy {
                configured_mode: options.configured_mode,
                plan: options.plan.clone(),
            })
            .content_authority(content_authority)
            .profiles(options.profiles)
            .subagent_max_concurrency(options.subagent_max_concurrency)
            .plan(options.plan)
            .session_defaults(
                options
                    .provider
                    .as_ref()
                    .map(|provider| provider.provider_id.clone()),
                options
                    .provider
                    .as_ref()
                    .map(|provider| provider.model.clone()),
            )
            .advisor_sink(Arc::new(BridgeAdvisorSink::new(Arc::clone(&bridge))))
            .build()
            .await
            .map_err(errors::runtime)?;
        let service = Self {
            core: Arc::new(Mutex::new(Some(Core {
                runtime,
                provider_ports,
                provider_memory_secrets,
                mcp_config_runtime: mcp_ports.0,
                mcp_config_secrets: mcp_ports.1,
                contributor,
                subscriptions: Arc::new(std::sync::Mutex::new(Vec::new())),
                http: None,
                mcp: None,
            }))),
            bridge,
            gate: Arc::new(CloseGate {
                state: std::sync::Mutex::new(GateState {
                    busy: false,
                    done: false,
                }),
                wake: Notify::new(),
            }),
            close_timeout: options.close_timeout,
            uncertain: Arc::new(AtomicBool::new(false)),
            env: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        };
        register_live(&service);
        Ok(service)
    }

    /// Register a JS-executed tool. `definitionJson` is the portable tool
    /// definition: `{name, version?, effect?, capability?, description?,
    /// inputSchema, outputSchema?, timeoutMs?}`.
    #[napi]
    pub async fn register_tool(&self, definition_json: String) -> Result<()> {
        let core_guard = self.core.lock().await;
        let core = core_guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        core.contributor
            .register(&definition_json)
            .map_err(errors::invalid)
    }

    /// Register the RUST coding toolpack as a first-class contributor on
    /// this service's executor lane: `read_file`, `list_dir`, `find_files`,
    /// `code_search`, `repo_map`, `git_inspect`, `edit_file`, `write_file`,
    /// `run_command`, `run_tests`, and `submit_plan` — the crate's own
    /// descriptors, executed by Rust, governed by the crate's own
    /// permission + high-risk guards over the binding's durable authority,
    /// with the crate's fingerprint-v4 standing approvals for the shell
    /// tools. Nothing on this path crosses into JS: no callback, no TS
    /// pack, no fallback.
    ///
    /// `optionsJson`: `{workspaceRoot (REQUIRED, absolute), sandbox?
    /// ("required"|"off", default "required"), network?, testCommands?,
    /// allowGitWrites?, allowSensitive?, configuredMode?
    /// ("read_only"|"ask"|"auto_edit"|"full_auto"|"plan", default "ask" —
    /// the mode used until the session's durable execution state says
    /// otherwise), role? ("implementer"|"reviewer"|"supervisor"),
    /// planToolNames? (default ["submit_plan"]), identity? (default the
    /// root; part of the standing-approval key envelope), coordinationDir?
    /// (default the per-uid shared lock dir), coordinationWaitMs?,
    /// environment? (string map replacing the host env for spawned commands
    /// and the approval key)}`.
    /// One-shot: a second registration is refused; the descriptor set of a
    /// service is immutable once the worker can claim turns.
    #[napi]
    pub async fn register_coding_tools(&self, options_json: String) -> Result<()> {
        let core_guard = self.core.lock().await;
        let core = core_guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        core.runtime
            .coding_lane()
            .register(&options_json)
            .await
            .map_err(errors::runtime)
    }

    /// Configure the subagent subsystem: `{enabled?, limits?:
    /// {maxDepth?, maxConcurrency?, maxPerParent?, awaitPollMs?,
    /// maxAwaitMs?, leaseTtlMs?, cancelOrphans?, maxTreeProviderRequests? (positive
    /// integer <= 1_000_000)}, roles?:
    /// {<id>: {description?, prompt?, allowedTools?, readOnly?,
    /// canSpawn?, model?, providerId?, maxIterations?, capability?}},
    /// delegationGuidance?}`. An omitted `roles` table ships the five
    /// built-ins (general, researcher, explorer, implementer, reviewer);
    /// partial tables keep the built-in defaults for any built-in role
    /// they name. Hot: applies to the next spawn.
    /// Disabled (the default) means no delegation tools exist at all.
    #[napi]
    pub async fn configure_subagents(&self, config_json: String) -> Result<()> {
        let core_guard = self.core.lock().await;
        let core = core_guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        let config: agentkit_agents::SubagentConfig = serde_json::from_str(&config_json)
            .map_err(|error| errors::invalid(format!("invalid subagent config: {error}")))?;
        if config
            .limits
            .max_tree_provider_requests
            .is_some_and(|limit| !(1..=1_000_000).contains(&limit))
        {
            return Err(errors::invalid(
                "invalid subagent config: maxTreeProviderRequests must be an integer in 1..=1000000",
            ));
        }
        core.runtime.subagent_lane().configure(config);
        Ok(())
    }

    /// JSON view of the active subagent configuration.
    #[napi]
    pub async fn subagent_config(&self) -> Result<String> {
        let core_guard = self.core.lock().await;
        let core = core_guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        Ok(core
            .runtime
            .subagent_lane()
            .view()
            .map(|value| value.to_string())
            .unwrap_or_else(|| "null".into()))
    }

    /// The children spawned by one parent chat: JSON array of the full
    /// child views `{childChatId, parentChatId, parentTaskId, role, depth,
    /// prompt, model, status, result, error, createdAt, updatedAt,
    /// turnTaskId, taskStatus, waiting, queuedInputs}` — the registry row
    /// joined with the live task state.
    #[napi]
    pub async fn subagents_list(&self, parent_chat_id: String) -> Result<String> {
        let core_guard = self.core.lock().await;
        let core = core_guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        let views = core
            .runtime
            .subagent_lane()
            .list(&parent_chat_id)
            .await
            .map_err(errors::agents)?;
        serde_json::to_string(&views).map_err(|error| errors::internal(&error.to_string()))
    }

    /// The state of one child session by id.
    #[napi]
    pub async fn subagent_status(&self, child_chat_id: String) -> Result<String> {
        let core_guard = self.core.lock().await;
        let core = core_guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        let view = core
            .runtime
            .subagent_lane()
            .status(&child_chat_id)
            .await
            .map_err(errors::agents)?;
        serde_json::to_string(&view).map_err(|error| errors::internal(&error.to_string()))
    }

    /// Register the LSP namespace: `{workspaceRoot (REQUIRED, absolute),
    /// config?: {servers?: {<language>: {command, args?, env?}},
    /// requestTimeoutMs?, initTimeoutMs?, idleTimeoutMs?,
    /// diagnosticsWaitMs?, maxRestarts?, restartWindowMs?,
    /// allowWorkspaceBinaries?}}`. Language servers are DISCOVERED only
    /// (explicit `servers` override → env → PATH; the workspace
    /// `node_modules/.bin` walk requires `allowWorkspaceBinaries: true`
    /// because running a binary that came with the repository is host
    /// code execution) — a missing server fails the tool call honestly;
    /// nothing is ever installed.
    #[napi]
    pub async fn register_lsp_tools(&self, options_json: String) -> Result<()> {
        let core_guard = self.core.lock().await;
        let core = core_guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        core.runtime
            .lsp_lane()
            .register(&options_json)
            .await
            .map_err(errors::runtime)
    }

    /// Configure advisor + watchdog: `{enabled?, model?, providerId?,
    /// watchdogProviderId?, watchdogModel?,
    /// maxIterations?, maxToolCallsPerIteration?, toolTimeoutMs?,
    /// providerRequests?, overallTimeoutMs?, maxFindings?,
    /// maxReviewConcurrency?, cooldownMs?, reviewsPerWindow?, windowMs?,
    /// triggerConsecutiveToolFailures?, triggerRepeatedErrorCode?,
    /// triggerEditCount?, triggerTurnFailures?, escalateCritical?}`.
    /// `model`/`watchdogModel` set the explicit-review and automatic-review
    /// models independently of the main agent's model. Hot: applies to the
    /// next review.
    #[napi]
    pub async fn configure_advisor(&self, config_json: String) -> Result<()> {
        let core_guard = self.core.lock().await;
        let core = core_guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        let config: agentkit_agents::AdvisorConfig = serde_json::from_str(&config_json)
            .map_err(|error| errors::invalid(format!("invalid advisor config: {error}")))?;
        core.runtime
            .advisor_lane()
            .validate(&config)
            .map_err(errors::runtime)?;
        core.runtime.advisor_lane().configure(config);
        Ok(())
    }

    /// JSON view of the active advisor configuration.
    #[napi]
    pub async fn advisor_config(&self) -> Result<String> {
        let core_guard = self.core.lock().await;
        let core = core_guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        Ok(core.runtime.advisor_lane().view().to_string())
    }

    /// Run one explicit advisory review of a session now (the `focus` is
    /// the reviewer's task framing). Returns the persisted report JSON.
    #[napi]
    pub async fn advisor_review(&self, session_id: String, focus: String) -> Result<String> {
        // Take only the lane handle. An explicit review is a full model
        // round-trip (seconds); holding the core mutex across it would
        // stall every other service call behind one reviewer.
        let lane = {
            let core_guard = self.core.lock().await;
            core_guard
                .as_ref()
                .ok_or_else(|| errors::closed("the session service is closed"))?
                .runtime
                .advisor_lane()
                .clone()
        };
        let report = lane
            .review(&session_id, focus)
            .await
            .map_err(errors::agents)?;
        serde_json::to_string(&report).map_err(|error| errors::internal(&error.to_string()))
    }

    /// Persisted advisor findings for a session, newest first.
    #[napi]
    pub async fn advisor_findings(&self, session_id: String, limit: Option<f64>) -> Result<String> {
        let limit = match limit {
            None => 50,
            Some(value) => integer_arg("limit", value, 1, i64::from(i32::MAX))?,
        };
        let core_guard = self.core.lock().await;
        let core = core_guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        let records = core
            .runtime
            .advisor_lane()
            .findings(&session_id, limit)
            .await
            .map_err(errors::agents)?;
        serde_json::to_string(&records).map_err(|error| errors::internal(&error.to_string()))
    }

    #[napi]
    pub async fn create_session(&self, input_json: String) -> Result<String> {
        let core_guard = self.core.lock().await;
        let core = core_guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        let input = parse_create_session(&input_json)?;
        let state = core
            .runtime
            .service()
            .create_session(&input)
            .await
            .map_err(errors::session)?;
        Ok(session_state_json(&state))
    }

    #[napi]
    pub async fn send(&self, session_id: String, input_json: String) -> Result<String> {
        let core_guard = self.core.lock().await;
        let core = core_guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        let input = parse_send_input(&input_json, WhenBusy::Reject)?;
        let result = core
            .runtime
            .service()
            .send(&session_id, &input)
            .await
            .map_err(errors::session)?;
        Ok(send_result_json(result))
    }

    #[napi]
    pub async fn follow_up(&self, session_id: String, input_json: String) -> Result<String> {
        let core_guard = self.core.lock().await;
        let core = core_guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        let input = parse_follow_up(&input_json)?;
        let result = core
            .runtime
            .service()
            .follow_up(&session_id, &input)
            .await
            .map_err(errors::session)?;
        Ok(send_result_json(result))
    }

    #[napi]
    pub async fn steer(&self, session_id: String, input_json: String) -> Result<String> {
        let core_guard = self.core.lock().await;
        let core = core_guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        let input = parse_steer(&input_json)?;
        let result = core
            .runtime
            .service()
            .steer(&session_id, &input)
            .await
            .map_err(errors::session)?;
        Ok(send_result_json(result))
    }

    /// Steer a LIVE turn only; an idle session answers `chat_idle` instead
    /// of starting a turn.
    #[napi]
    pub async fn steer_live(&self, session_id: String, input_json: String) -> Result<String> {
        let core_guard = self.core.lock().await;
        let core = core_guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        let input = parse_steer(&input_json)?;
        let result = core
            .runtime
            .service()
            .steer_live(&session_id, &input)
            .await
            .map_err(errors::session)?;
        Ok(send_result_json(result))
    }

    #[napi]
    pub async fn get_state(&self, session_id: String) -> Result<String> {
        let core_guard = self.core.lock().await;
        let core = core_guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        let state = core
            .runtime
            .service()
            .get_state(&session_id)
            .await
            .map_err(errors::session)?;
        Ok(session_state_json(&state))
    }

    #[napi]
    pub async fn get_execution(&self, session_id: String) -> Result<Option<String>> {
        let core_guard = self.core.lock().await;
        let core = core_guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        let execution = core
            .runtime
            .service()
            .get_execution(&session_id)
            .await
            .map_err(errors::session)?;
        Ok(execution.map(|value| value.to_string()))
    }

    #[napi]
    pub async fn list_turns(&self, session_id: String) -> Result<String> {
        let core_guard = self.core.lock().await;
        let core = core_guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        let turns = core
            .runtime
            .service()
            .list_turns(&session_id)
            .await
            .map_err(errors::session)?;
        Ok(Value::Array(turns).to_string())
    }

    /// The durable projection of one session (turns, messages, inputs,
    /// descriptor and execution metadata) — the reopened-history view.
    /// Private tool execution receipt rows are not included.
    #[napi]
    pub async fn export_session(&self, session_id: String) -> Result<String> {
        let core_guard = self.core.lock().await;
        let core = core_guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        let exported = core
            .runtime
            .service()
            .export_session(&session_id)
            .await
            .map_err(errors::session)?;
        Ok(exported.to_string())
    }

    #[napi]
    pub async fn get_usage(&self, session_id: String) -> Result<String> {
        let core_guard = self.core.lock().await;
        let core = core_guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        let usage = core
            .runtime
            .service()
            .get_usage(&session_id)
            .await
            .map_err(errors::session)?;
        Ok(usage.to_string())
    }

    /// Answer a pending human request. `inputJson` is
    /// `{requestId, answerId, response}`.
    #[napi]
    pub async fn answer(&self, input_json: String) -> Result<String> {
        let core_guard = self.core.lock().await;
        let core = core_guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        let input = parse_answer(&input_json)?;
        let receipt = core
            .runtime
            .service()
            .answer(&input)
            .await
            .map_err(errors::session)?;
        Ok(receipt_json(&receipt))
    }

    #[napi]
    pub async fn approve(
        &self,
        request_id: String,
        answer_id: String,
        note: Option<String>,
        remember: Option<bool>,
    ) -> Result<String> {
        let core_guard = self.core.lock().await;
        let core = core_guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        let receipt = core
            .runtime
            .service()
            .approve(&request_id, &answer_id, note.as_deref(), remember)
            .await
            .map_err(errors::session)?;
        Ok(receipt_json(&receipt))
    }

    #[napi]
    pub async fn deny(
        &self,
        request_id: String,
        answer_id: String,
        note: Option<String>,
    ) -> Result<String> {
        let core_guard = self.core.lock().await;
        let core = core_guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        let receipt = core
            .runtime
            .service()
            .deny(&request_id, &answer_id, note.as_deref())
            .await
            .map_err(errors::session)?;
        Ok(receipt_json(&receipt))
    }

    #[napi]
    pub async fn cancel(&self, session_id: String, turn_id: String) -> Result<()> {
        let core_guard = self.core.lock().await;
        let core = core_guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        core.runtime
            .service()
            .cancel(&session_id, &turn_id)
            .await
            .map_err(errors::session)
    }

    #[napi]
    pub async fn interrupt(&self, session_id: String, turn_id: String) -> Result<()> {
        let core_guard = self.core.lock().await;
        let core = core_guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        core.runtime
            .service()
            .interrupt(&session_id, &turn_id)
            .await
            .map_err(errors::session)
    }

    #[napi]
    pub async fn resume(&self, session_id: String, turn_id: String) -> Result<()> {
        let core_guard = self.core.lock().await;
        let core = core_guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        core.runtime
            .service()
            .resume(&session_id, &turn_id)
            .await
            .map_err(errors::session)
    }

    #[napi]
    pub async fn flush_queue(&self, session_id: String) -> Result<Option<String>> {
        let core_guard = self.core.lock().await;
        let core = core_guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        let result = core
            .runtime
            .service()
            .flush_queue(&session_id)
            .await
            .map_err(errors::session)?;
        Ok(result.map(send_result_json))
    }

    /// Subscribe to the durable committed event stream of one turn task.
    /// Events are handed out in `seq` order; a gap is impossible by
    /// construction because every poll continues from the last delivered
    /// cursor and each event's `seq` is returned to the caller.
    #[napi]
    pub async fn subscribe_turn(
        &self,
        task_id: String,
        after_seq: Option<f64>,
    ) -> Result<SubscriptionHandle> {
        let core_guard = self.core.lock().await;
        let core = core_guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        let after = match after_seq {
            None => None,
            Some(value) if value.is_finite() && value >= 0.0 && value.fract() == 0.0 => {
                Some(value as u64)
            }
            Some(_) => return Err(errors::invalid("afterSeq must be a non-negative integer")),
        };
        let run: RunHandle = core.runtime.host().run_handle(&task_id);
        let receiver = run.subscribe(after);
        let subscription = Arc::new(Subscription {
            receiver: Mutex::new(Some(receiver)),
            closed: AtomicBool::new(false),
            cancellation: CancellationToken::new(),
            registry: Arc::downgrade(&core.subscriptions),
        });
        core.subscriptions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(Arc::clone(&subscription));
        Ok(SubscriptionHandle {
            subscription,
            buffer: std::sync::Mutex::new(Vec::new()),
            deferred_error: std::sync::Mutex::new(None),
        })
    }

    /// Tag this service with the calling Node environment and make sure that
    /// environment has a cleanup hook (once per env). The JS wrapper calls it
    /// right after construction; async constructors have no `Env`, so this
    /// is the earliest synchronous point. Idempotent.
    #[napi(catch_unwind)]
    pub fn bind_env(&self, env: Env) -> Result<()> {
        let raw = env.raw() as usize;
        self.env.store(raw, Ordering::SeqCst);
        let mut hooked = HOOKED
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !hooked.contains(&raw) {
            env.add_env_cleanup_hook(raw, |raw| {
                HOOKED
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .retain(|candidate| *candidate != raw);
                shutdown_live_services(raw);
            })?;
            hooked.push(raw);
        }
        Ok(())
    }

    /// Release the event-loop reference held by the callback delivery
    /// function. The JS wrapper calls this first in `close()`; after it, no
    /// new callback can wake the loop, so a process whose script has ended
    /// can actually exit even while the class object is still referenced.
    #[napi(catch_unwind)]
    pub fn detach(&self, env: Env) -> Result<()> {
        self.bridge
            .detach(&env)
            .map_err(|_| errors::internal("bridge detach"))
    }

    /// Shutdown: `Open → Closing → Closed` in one atomic sequence.
    /// Entering Closing rejects outstanding callbacks AND new JS dispatches
    /// under the same lock (no insert-after-drain window), then the service
    /// stops taking operations, cancels subscriptions, and awaits the
    /// worker. Active runs observe the closing bridge, fail their next
    /// callback honestly, and settle durably — close never fakes success,
    /// and rejecting a JS promise never claims to stop external effects the
    /// callback already started. If the worker does not finish draining
    /// within `closeTimeoutMs`, close still completes and `closeUncertain`
    /// reports that cleanup could not be established. Idempotent;
    /// concurrent callers await the same completion.
    ///
    /// SHUTDOWN SCOPE — what `close()` does and does not promise:
    /// * It stops the runtime: no new dispatch, no new runs, worker drained
    ///   (or `closeUncertain` records the unproven drain). A running desktop
    ///   HTTP server is stopped before Closing begins, so no REST/SSE
    ///   request is admitted into the shutdown window.
    /// * It does NOT quiesce external effects a dispatched JS callback
    ///   already started. A callback handed to JS before Closing may
    ///   continue acting after `close()` resolves; the runtime only
    ///   guarantees it never fabricates that effect's outcome — the durable
    ///   receipt stays admitted-without-result and recovery treats it as
    ///   uncertain. Use the JS wrapper's `pendingCallbacks` to observe
    ///   dispatched, unsettled callbacks before relying on any stronger
    ///   shutdown claim.
    #[napi]
    pub async fn close(&self) -> Result<()> {
        loop {
            let mut waited = Box::pin(self.gate.wake.notified());
            waited.as_mut().enable();
            let action = {
                let mut state = self.gate.lock();
                if state.done {
                    GateAction::Done
                } else if state.busy {
                    GateAction::Wait
                } else {
                    state.busy = true;
                    GateAction::Run
                }
            };
            match action {
                GateAction::Done => return Ok(()),
                GateAction::Wait => {
                    waited.await;
                    continue;
                }
                GateAction::Run => {}
            }
            let mut busy = BusyGuard {
                gate: &self.gate,
                done: false,
            };
            let outcome = self.shutdown_once().await;
            busy.done = outcome.is_ok();
            return outcome;
        }
    }

    async fn shutdown_once(&self) -> Result<()> {
        // Stop the desktop HTTP and MCP listeners FIRST: with the handles
        // taken under the same lock that gates `startHttpServer`/
        // `startMcpServer`, no REST/SSE or MCP request can be admitted (or
        // re-admitted) once Closing begins. The accept loops are awaited
        // outside the lock so draining never blocks a concurrent service
        // method. The MCP close revokes every session lease first, which
        // cancels in-flight JS tool callbacks dispatched by remote calls.
        let mut core = self.core.lock().await.take();
        if let Some(core) = core.as_mut() {
            stop_servers(core, self.close_timeout).await;
        }
        self.bridge.start_closing();
        // Cleanup is unconditional: a worker error is remembered and
        // returned only AFTER lanes are disposed and the bridge is closed,
        // so a failed drain can never leave commands or language servers
        // running behind a "closed" service.
        let outcome = match core {
            Some(core) => drain_core(core, self.close_timeout, &self.uncertain).await,
            None => Ok(()),
        };
        self.bridge.mark_closed();
        outcome
    }

    /// True when shutdown could not establish worker drain or durable
    /// completion of owned native command receipts. A deadline or receipt
    /// persistence failure remains uncertain; durable state stays authoritative.
    /// This never proves external JavaScript callback effects stopped; the
    /// wrapper's `pendingCallbacks` reports dispatched, unsettled callbacks.
    #[napi(getter, catch_unwind)]
    pub fn close_uncertain(&self) -> bool {
        self.uncertain.load(Ordering::SeqCst)
    }
}

/// Stop the desktop listeners (before the bridge starts Closing).
async fn stop_servers(core: &mut Core, grace: Duration) {
    if let Some(server) = core.http.take() {
        server.shutdown(grace).await;
    }
    if let Some(server) = core.mcp.take() {
        server.shutdown(grace).await;
    }
}

/// Everything after the bridge is Closing: cancel subscriptions, drain the
/// worker (bounded), then dispose the coding lane and the language servers.
/// Always runs to the end; returns the first worker error afterwards.
async fn drain_core(mut core: Core, close_timeout: Duration, uncertain: &AtomicBool) -> Result<()> {
    let mut first_error: Option<napi::Error> = None;
    cancel_all(&core.subscriptions);
    if let Some(worker) = core.runtime.take_worker()
        && let Err(error) =
            drain_worker(worker, core.runtime.coding_lane(), close_timeout, uncertain).await
    {
        first_error = Some(error);
    }
    // Native coding commands this process owns get the crate's own
    // teardown (drain + scratch removal) once no new dispatch can
    // start; anything a DEAD process left is the next boot's
    // startup sweep, not ours. Outcomes stay whatever the receipt
    // log decides - teardown never claims them.
    core.runtime.coding_lane().mark_disposed();
    core.runtime.coding_lane().dispose().await;
    if let Err(error) =
        agentkit_chat::drain_native_command_completions(core.runtime.host().store(), close_timeout)
            .await
    {
        uncertain.store(true, Ordering::SeqCst);
        if first_error.is_none() {
            first_error = Some(errors::core(error));
        }
    }
    // Language servers get a clean shutdown (exit + kill fallback)
    // once no new dispatch can start; a dead process's leftovers
    // die with the process, not with us.
    core.runtime.lsp_lane().close().await;
    drop(core);
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// Upper bound on the hard-stop wait after the graceful deadline.
const HARD_STOP_WAIT: Duration = Duration::from_secs(5);

/// Graceful drain, then a hard stop. `stop` + `join` instead of `shutdown`
/// keeps the handle alive past the first deadline so it can be escalated.
async fn drain_worker(
    mut worker: agentkit_sessions::WorkerHandle,
    lane: &CodingLane,
    close_timeout: Duration,
    uncertain: &AtomicBool,
) -> Result<()> {
    // The whole wait, including the admission lock, is inside the deadline.
    let graceful = tokio::time::timeout(close_timeout, async {
        worker.stop().await;
        worker.join().await
    })
    .await;
    let Err(_) = graceful else {
        return graceful.unwrap_or(Ok(())).map_err(errors::host);
    };
    // No tool body may start once close() resolves; refuse it first, then
    // fire runtime-only cancellation (no durable Cancelled) so the turn stays
    // recoverable like a crashed one.
    lane.mark_disposed();
    let second = close_timeout.min(HARD_STOP_WAIT);
    let hard = tokio::time::timeout(second, async {
        worker.interrupt_active().await;
        worker.join().await
    })
    .await;
    match hard {
        Ok(result) => result.map_err(errors::host),
        Err(_) => {
            // Even the hard stop did not drain: record it and make sure
            // nothing keeps running detached.
            uncertain.store(true, Ordering::SeqCst);
            worker.abort();
            Ok(())
        }
    }
}

fn cancel_all(subscriptions: &Subscriptions) {
    // Drain under the lock, cancel outside it: `Subscription::cancel`
    // prunes itself from this same list.
    let drained: Vec<Arc<Subscription>> = subscriptions
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .drain(..)
        .collect();
    for subscription in drained {
        subscription.cancel();
    }
}

/// Teardown without a `close()` caller: listeners are cut at once and run
/// CONCURRENTLY with the drain, so worker stop, lane disposal and language
/// server shutdown always get their (bounded) turn.
async fn fast_teardown(
    mut core: Core,
    bridge: Arc<JsBridge>,
    close_timeout: Duration,
    uncertain: Arc<AtomicBool>,
) {
    let http = core.http.take();
    let mcp = core.mcp.take();
    let servers = async {
        if let Some(server) = http {
            server.shutdown_now().await;
        }
        if let Some(server) = mcp {
            server.shutdown_now().await;
        }
    };
    let _ = tokio::join!(servers, drain_core(core, close_timeout, &uncertain));
    bridge.mark_closed();
}

/// Best-effort teardown for a service that was never `close()`d. Never
/// blocks: the bridge starts closing synchronously, the rest runs on the
/// binding's runtime.
fn teardown_in_background(
    core: Core,
    bridge: Arc<JsBridge>,
    close_timeout: Duration,
    uncertain: Arc<AtomicBool>,
) {
    bridge.start_closing();
    let task = fast_teardown(core, bridge, close_timeout, uncertain);
    // A runtime that is already gone (late env teardown) cannot take the
    // task; the bridge fence above is all that can still be done.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        napi::bindgen_prelude::spawn(task);
    }));
}

impl Drop for SessionService {
    fn drop(&mut self) {
        let core = match self.core.try_lock() {
            Ok(mut guard) => guard.take(),
            Err(_) => None,
        };
        let Some(core) = core else {
            return;
        };
        teardown_in_background(
            core,
            Arc::clone(&self.bridge),
            self.close_timeout,
            Arc::clone(&self.uncertain),
        );
    }
}

/// Services alive in this process, for the environment cleanup hook: Node
/// tears an environment down without running Rust destructors for objects
/// still referenced from JS, so the hook is the only place a never-closed
/// service can be fenced before the process (or worker thread) goes away.
struct Live {
    env: Arc<std::sync::atomic::AtomicUsize>,
    bridge: Weak<JsBridge>,
    core: Weak<Mutex<Option<Core>>>,
    close_timeout: Duration,
    uncertain: Weak<AtomicBool>,
}

static LIVE: std::sync::Mutex<Vec<Live>> = std::sync::Mutex::new(Vec::new());

fn register_live(service: &SessionService) {
    let mut live = LIVE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    live.retain(|entry| entry.core.strong_count() > 0);
    live.push(Live {
        env: Arc::clone(&service.env),
        bridge: Arc::downgrade(&service.bridge),
        core: Arc::downgrade(&service.core),
        close_timeout: service.close_timeout,
        uncertain: Arc::downgrade(&service.uncertain),
    });
}

/// Environments that already carry a cleanup hook (one per Node env).
static HOOKED: std::sync::Mutex<Vec<usize>> = std::sync::Mutex::new(Vec::new());

/// Environment-cleanup entry point: fence and stop every service of `env`
/// that is still open. Bounded - Node is waiting on this thread.
fn shutdown_live_services(env: usize) {
    // Only this environment's services: a worker thread exiting must not
    // tear down the main thread's (or a sibling worker's) services.
    let entries: Vec<Live> = {
        let mut live = LIVE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let (mine, others): (Vec<Live>, Vec<Live>) = std::mem::take(&mut *live)
            .into_iter()
            .partition(|entry| entry.env.load(Ordering::SeqCst) == env);
        *live = others;
        mine
    };
    let mut jobs = Vec::new();
    for entry in entries {
        let (Some(bridge), Some(core), Some(uncertain)) = (
            entry.bridge.upgrade(),
            entry.core.upgrade(),
            entry.uncertain.upgrade(),
        ) else {
            continue;
        };
        bridge.start_closing();
        let taken = match core.try_lock() {
            Ok(mut guard) => guard.take(),
            Err(_) => None,
        };
        if let Some(taken) = taken {
            jobs.push((taken, bridge, entry.close_timeout, uncertain));
        }
    }
    if jobs.is_empty() {
        return;
    }
    let cap = Duration::from_millis(2_000);
    // A private current-thread runtime: napi's own runtime may already be
    // shut down by the time Node runs this hook (hooks run in reverse
    // registration order), and tasks it owned died with it.
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return;
    };
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        runtime.block_on(async move {
            let mut tasks = Vec::new();
            for (core, bridge, timeout, uncertain) in jobs {
                tasks.push(fast_teardown(core, bridge, timeout.min(cap), uncertain));
            }
            let _ = tokio::time::timeout(cap, futures::future::join_all(tasks)).await;
        })
    }));
    // Blocking tasks (spawn_blocking) must not hold the hook past the cap.
    runtime.shutdown_timeout(cap);
}

/// One turn subscription. `poll` drains the next committed events; the
/// stream is pull-based, so a slow consumer applies backpressure to the
/// follower instead of overflowing a queue.
#[napi(js_name = "SubscriptionHandle")]
pub struct SubscriptionHandle {
    subscription: Arc<Subscription>,
    buffer: std::sync::Mutex<Vec<Value>>,
    /// An error that arrived behind already-popped events. The batch goes
    /// out first; the error is reported by the next poll instead of
    /// discarding events the follower cannot re-read.
    deferred_error: std::sync::Mutex<Option<(Status, String)>>,
}

impl Drop for SubscriptionHandle {
    fn drop(&mut self) {
        self.subscription.cancel();
    }
}

struct Subscription {
    receiver: Mutex<Option<mpsc::Receiver<HostResult<RunUpdate>>>>,
    closed: AtomicBool,
    /// Wakes a `poll` that is parked on the receiver: `cancel` must not
    /// depend on winning the receiver lock (a parked poll holds it).
    cancellation: CancellationToken,
    registry: Weak<std::sync::Mutex<Vec<Arc<Subscription>>>>,
}

impl Subscription {
    fn cancel(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.cancellation.cancel();
        if let Ok(mut guard) = self.receiver.try_lock() {
            let _ = guard.take();
        }
        self.prune();
    }

    /// Remove this subscription from its service's list (a finished or
    /// cancelled subscription must not stay pinned there for the life of
    /// the service).
    fn prune(&self) {
        if let Some(registry) = self.registry.upgrade() {
            registry
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .retain(|candidate| !std::ptr::eq(Arc::as_ptr(candidate), self));
        }
    }
}

#[napi]
impl SubscriptionHandle {
    /// Drain up to `limit` (max 64) committed events, waiting up to
    /// `waitMs` for at least one when idle. `done` means the turn settled
    /// (or failed/closed) and no further events will arrive.
    #[napi]
    pub async fn poll(&self, limit: Option<f64>, wait_ms: Option<f64>) -> Result<String> {
        let limit = match limit {
            None => MAX_POLL_BATCH,
            Some(value) => integer_arg("limit", value, 1, i64::from(MAX_POLL_BATCH))
                .map_err(|_| errors::invalid("limit must be an integer between 1 and 64"))?
                as i32,
        };
        let wait = wait_ms
            .filter(|value| value.is_finite() && *value >= 0.0)
            .unwrap_or(DEFAULT_POLL_WAIT_MS as f64)
            .min(30_000.0) as u64;
        let events = std::mem::take(
            self.buffer
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .deref_mut(),
        );
        if events.is_empty()
            && let Some((status, reason)) = self
                .deferred_error
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take()
        {
            return Err(Error::new(status, reason));
        }
        if events.len() >= usize::try_from(limit).unwrap_or(1) {
            self.finish_batch(events)
        } else {
            self.poll_more(limit, wait, events).await
        }
    }

    /// Stop the subscription. Idempotent; polling afterwards reports
    /// `done` with no events.
    #[napi(catch_unwind)]
    pub fn cancel(&self) {
        self.subscription.cancel();
    }

    #[napi(getter, catch_unwind)]
    pub fn done(&self) -> bool {
        self.subscription.closed.load(Ordering::SeqCst)
    }
}

impl SubscriptionHandle {
    fn defer_error(&self, error: Error) {
        *self
            .deferred_error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some((error.status, error.reason));
    }

    fn finish_batch(&self, mut events: Vec<Value>) -> Result<String> {
        let done = self.subscription.closed.load(Ordering::SeqCst);
        if done {
            self.subscription.prune();
        }
        let limit = events.len();
        if !done && limit > usize::try_from(MAX_POLL_BATCH).unwrap_or(64) {
            // Cannot happen through `poll`, but the contract holds: excess
            // stays buffered for the next call.
            let keep = events.split_off(usize::try_from(MAX_POLL_BATCH).unwrap_or(64));
            *self
                .buffer
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = keep;
        }
        Ok(json!({ "events": Value::Array(events), "done": done }).to_string())
    }

    async fn poll_more(&self, limit: i32, wait_ms: u64, mut events: Vec<Value>) -> Result<String> {
        let cap = usize::try_from(limit).unwrap_or(1);
        let mut guard = self.subscription.receiver.lock().await;
        let Some(receiver) = guard.as_mut() else {
            self.subscription.closed.store(true, Ordering::SeqCst);
            drop(guard);
            return self.finish_batch(events);
        };
        loop {
            if events.len() >= cap {
                break;
            }
            match receiver.try_recv() {
                Ok(update) => match drain_update(update) {
                    Ok(Drain::Event(value)) => events.push(value),
                    Ok(Drain::Done(value)) => {
                        if let Some(value) = value {
                            events.push(value);
                        }
                        self.subscription.closed.store(true, Ordering::SeqCst);
                        break;
                    }
                    Err(error) => {
                        if events.is_empty() {
                            return Err(error);
                        }
                        self.defer_error(error);
                        break;
                    }
                },
                Err(_) => break,
            }
        }
        if events.is_empty() && !self.subscription.closed.load(Ordering::SeqCst) {
            // Wait for the first item so idle turns do not spin the JS side.
            let waited = tokio::select! {
                () = self.subscription.cancellation.cancelled() => None,
                waited = tokio::time::timeout(
                    std::time::Duration::from_millis(wait_ms),
                    receiver.recv(),
                ) => Some(waited),
            };
            match waited {
                None => {
                    self.subscription.closed.store(true, Ordering::SeqCst);
                }
                Some(Ok(Some(update))) => match drain_update(update)? {
                    Drain::Event(value) => events.push(value),
                    Drain::Done(value) => {
                        if let Some(value) = value {
                            events.push(value);
                        }
                        self.subscription.closed.store(true, Ordering::SeqCst);
                    }
                },
                Some(Ok(None)) => {
                    self.subscription.closed.store(true, Ordering::SeqCst);
                }
                Some(Err(_elapsed)) => {}
            }
        }
        if self.subscription.closed.load(Ordering::SeqCst) {
            // Done or cancelled: release the receiver (and with it the run
            // follower) now rather than at handle drop.
            let _ = guard.take();
        }
        drop(guard);
        self.finish_batch(events)
    }
}

enum Drain {
    Event(Value),
    Done(Option<Value>),
}

fn drain_update(update: HostResult<RunUpdate>) -> Result<Drain> {
    match update {
        Ok(RunUpdate::Event(record)) => {
            let value = serde_json::to_value(&record)
                .map_err(|_| errors::internal("run event serialization"))?;
            Ok(Drain::Event(json!({ "type": "event", "event": value })))
        }
        Ok(RunUpdate::Settled(task)) => {
            let value = serde_json::to_value(&task)
                .map_err(|_| errors::internal("settled task serialization"))?;
            Ok(Drain::Done(Some(
                json!({ "type": "settled", "task": value }),
            )))
        }
        Err(error) => Err(errors::host(error)),
    }
}

// ---------------------------------------------------------------------------
// JSON input mapping (the JS wrapper owns the public camelCase surface; the
// binding parses here so bad input fails with `invalid_input`, not a panic)
// ---------------------------------------------------------------------------

use agentkit_sessions::{
    AgentSendResult, AnswerInput, CreateSessionInput, FollowUpInput, OnSettled, SendInput,
    SteerInput, WhenBusy,
};

/// Validate a JS number destined for an integer parameter: finite, whole,
/// and inside `[min, max]`. `f64 -> i32` casts would silently saturate or
/// truncate (NaN becomes 0); an out-of-contract value is `invalid_input`.
fn integer_arg(name: &str, value: f64, min: i64, max: i64) -> Result<i64> {
    if value.is_finite() && value.fract() == 0.0 && value >= min as f64 && value <= max as f64 {
        Ok(value as i64)
    } else {
        Err(errors::invalid(format!(
            "{name} must be an integer between {min} and {max}"
        )))
    }
}

fn parse_object(text: &str, what: &str) -> Result<Value> {
    serde_json::from_str::<Value>(text)
        .ok()
        .filter(|value| value.is_object())
        .ok_or_else(|| errors::invalid(format!("{what} must be a JSON object")))
}

fn optional_string(object: &Value, key: &str) -> Result<Option<String>> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(errors::invalid(format!(
            "{key} must be a string when present"
        ))),
    }
}

/// Descriptor-raw scalar fields: an explicit `null` is a malformed-present
/// value (the native descriptor reader's direction), never laundered into
/// an absence by the binding.
fn optional_string_strict(object: &Value, key: &str) -> Result<Option<String>> {
    match object.get(key) {
        None => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(Value::Null) => Err(errors::invalid(format!(
            "{key} must be a string; explicit null is malformed, not absent"
        ))),
        Some(_) => Err(errors::invalid(format!(
            "{key} must be a string when present"
        ))),
    }
}

/// Construction-time service configuration. Everything here selects Rust
/// behavior; an unsupported value fails construction rather than being
/// silently ignored, so the binding never advertises a policy it cannot
/// enforce.
struct ServiceOptions {
    profiles: Vec<Value>,
    close_timeout: Duration,
    configured_mode: CodingMode,
    plan: PlanConfiguration,
    provider: Option<ProviderLane>,
    /// Extra routable provider lanes (`additionalProviders`); each is built
    /// exactly like the primary configured lane.
    additional_providers: Vec<ProviderLane>,
    /// The JS callback lane's declared parameter capabilities (P: account
    /// parity). `true` means the callback wraps an account-backed client
    /// that REJECTS sampling/output parameters: admission then never
    /// injects an implicit output cap (TS reference: `bounded-context-client`
    /// enforces the ceiling only when the request carries one, for
    /// `chatgpt-account` clients), and the bridge payload omits options the
    /// adapter does not declare. The API-key lane (default) is unchanged.
    account_backed_provider: bool,
    /// START-TIME subagent `maxConcurrency`: sizes the worker pool (parents
    /// hold a slot while awaiting children) and is the ceiling later
    /// `configureSubagents` calls are clamped to.
    subagent_max_concurrency: i64,
}

/// The Rust-native provider lane selected by `providerConfig`. A
/// construction-time-only value: when present, the binding installs the core
/// preset (`chat-completions` or `responses`) over a registered connection
/// instead of the JS callback provider. Credentials are NOT part of this —
/// they live only in the connection registry, keyed by `connection_id`.
#[derive(Debug)]
struct ProviderLane {
    protocol: ProviderProtocol,
    provider_id: String,
    connection_id: String,
    /// Validated here; consumed by the TS wrapper as the session's default
    /// model (tasks carry the session's durable model from there).
    #[allow(dead_code)]
    model: String,
    temperature: Option<f64>,
    max_tokens: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProviderProtocol {
    ChatCompletions,
    Responses,
}

impl ProviderProtocol {
    fn as_str(self) -> &'static str {
        match self {
            ProviderProtocol::ChatCompletions => "chat-completions",
            ProviderProtocol::Responses => "responses",
        }
    }
}

impl Default for ServiceOptions {
    fn default() -> Self {
        Self {
            profiles: vec![],
            close_timeout: Duration::from_millis(DEFAULT_CLOSE_TIMEOUT_MS),
            configured_mode: CodingMode::Ask,
            plan: PlanConfiguration {
                tool_names: vec!["submit_plan".to_owned()],
                granted_mode: CodingMode::Ask,
            },
            provider: None,
            additional_providers: Vec::new(),
            account_backed_provider: false,
            subagent_max_concurrency: agentkit_agents::SubagentConfig::default()
                .limits
                .max_concurrency,
        }
    }
}

fn parse_service_options(text: Option<&str>) -> Result<ServiceOptions> {
    let Some(text) = text else {
        return Ok(ServiceOptions::default());
    };
    let object = parse_object(text, "the service options")?;
    let mut options = ServiceOptions::default();
    if let Some(value) = object.get("profiles") {
        options.profiles = value
            .as_array()
            .cloned()
            .ok_or_else(|| errors::invalid("profiles must be an array when present"))?;
        // Validated at construction by the host's own rules; a rejected
        // definition is a caller error (`invalid_input`), never an internal one.
        agentkit_sessions::ProfileRegistry::new(&options.profiles)
            .map_err(|error| errors::invalid(format!("profiles: {error}")))?;
    }
    if let Some(value) = object.get("closeTimeoutMs") {
        let millis = value
            .as_u64()
            .filter(|value| *value <= MAX_CLOSE_TIMEOUT_MS)
            .ok_or_else(|| {
                errors::invalid(format!(
                    "closeTimeoutMs must be an integer between 0 and {MAX_CLOSE_TIMEOUT_MS}"
                ))
            })?;
        options.close_timeout = Duration::from_millis(millis);
    }
    if let Some(value) = object.get("jsToolAuthority") {
        let authority = value
            .as_object()
            .ok_or_else(|| errors::invalid("jsToolAuthority must be an object when present"))?;
        if let Some(mode) = authority.get("configuredMode") {
            options.configured_mode = parse_mode(mode, "configuredMode")?;
        }
        if let Some(mode) = authority.get("planGrantMode") {
            let granted = parse_mode(mode, "planGrantMode")?;
            if granted == CodingMode::Plan {
                return Err(errors::invalid(
                    "planGrantMode must move the session out of plan mode, not into it",
                ));
            }
            options.plan.granted_mode = granted;
        }
        if let Some(names) = authority.get("planToolNames") {
            let items = names
                .as_array()
                .ok_or_else(|| errors::invalid("planToolNames must be an array when present"))?;
            if items.is_empty() {
                return Err(errors::invalid("planToolNames must not be empty"));
            }
            let mut parsed = Vec::with_capacity(items.len());
            for item in items {
                let name = item
                    .as_str()
                    .filter(|text| !text.is_empty())
                    .ok_or_else(|| {
                        errors::invalid("planToolNames entries must be non-empty strings")
                    })?;
                if !parsed.iter().any(|existing: &String| existing == name) {
                    parsed.push(name.to_owned());
                }
            }
            options.plan.tool_names = parsed;
        }
    }
    if let Some(value) = object.get("subagentMaxConcurrency") {
        options.subagent_max_concurrency = value
            .as_i64()
            .filter(|value| (1..=MAX_SUBAGENT_POOL).contains(value))
            .ok_or_else(|| {
                errors::invalid(format!(
                    "subagentMaxConcurrency must be an integer between 1 and {MAX_SUBAGENT_POOL}"
                ))
            })?;
    }
    if let Some(value) = object.get("providerConfig") {
        options.provider = Some(parse_provider_config(value)?);
    }
    if let Some(value) = object.get("additionalProviders") {
        options.additional_providers = parse_additional_providers(value, &options.provider)?;
    }
    if let Some(value) = object.get("providerAuthKind") {
        let kind = value
            .as_str()
            .filter(|text| *text == "api-key" || *text == "chatgpt-account")
            .ok_or_else(|| {
                errors::invalid("providerAuthKind must be \"api-key\" or \"chatgpt-account\"")
            })?;
        if kind == "chatgpt-account" && options.provider.is_some() {
            return Err(errors::invalid(
                "providerAuthKind \"chatgpt-account\" describes the JS callback lane; the Rust lane over providerConfig is API-key shaped",
            ));
        }
        options.account_backed_provider = kind == "chatgpt-account";
    }
    Ok(options)
}

/// Parse `providerConfig`: `{protocol, providerId, connectionId, model,
/// temperature?, maxTokens?}`. Every field must be well-formed — a
/// half-configured Rust lane fails construction (`invalid_input`, or
/// `invalid_connection` for empty ids), never silently falling back to the
/// JS provider as if the configuration had been accepted.
fn parse_provider_config(value: &Value) -> Result<ProviderLane> {
    let object = value
        .as_object()
        .ok_or_else(|| errors::invalid("providerConfig must be an object when present"))?;
    let protocol = match object.get("protocol") {
        Some(Value::String(text)) if text == "chat-completions" => {
            ProviderProtocol::ChatCompletions
        }
        Some(Value::String(text)) if text == "responses" => ProviderProtocol::Responses,
        Some(Value::String(text)) => {
            return Err(errors::invalid(format!(
                "unknown providerConfig.protocol \"{text}\" (expected \"chat-completions\" or \"responses\")"
            )));
        }
        _ => {
            return Err(errors::invalid(
                "providerConfig.protocol must be \"chat-completions\" or \"responses\"",
            ));
        }
    };
    let required = |key: &str| -> Result<String> {
        object
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| errors::invalid(format!("providerConfig.{key} must be a string")))
    };
    let provider_id = required("providerId")?;
    if provider_id.is_empty() {
        return Err(errors::connection(
            "providerConfig.providerId must be non-empty",
        ));
    }
    let connection_id = required("connectionId")?;
    if connection_id.is_empty() {
        return Err(errors::connection(
            "providerConfig.connectionId must be non-empty",
        ));
    }
    // The lane's model is the default the TS wrapper stamps onto sessions on
    // this lane; the request itself carries the session's durable model.
    let model = required("model")?;
    if model.is_empty() {
        return Err(errors::invalid("providerConfig.model must be non-empty"));
    }
    let temperature = match object.get("temperature") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_f64()
                .filter(|value| value.is_finite())
                .ok_or_else(|| {
                    errors::invalid("providerConfig.temperature must be a finite number")
                })?,
        ),
    };
    let max_tokens = match object.get("maxTokens") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_u64()
                .filter(|value| *value > 0 && *value <= 9_007_199_254_740_991)
                .ok_or_else(|| {
                    // Service construction is the earliest layer that sees the
                    // value, so it is the layer that must refuse it: a cap that
                    // admission could not represent never reaches the request
                    // (P1-F — the admitted request is the transmitted request).
                    errors::invalid("providerConfig.maxTokens must be a positive safe integer")
                })?,
        ),
    };
    Ok(ProviderLane {
        protocol,
        provider_id,
        connection_id,
        model,
        temperature,
        max_tokens,
    })
}

/// The JS callback provider's `ChatProvider::id`; the primary's id when no
/// `providerConfig` lane is installed.
const JS_PROVIDER_ID: &str = "js";
const MAX_ADDITIONAL_PROVIDERS: usize = 16;

/// Parse `additionalProviders`: up to 16 lanes of the `providerConfig`
/// shape, with ids unique and distinct from the primary's. A duplicate would
/// make routing ambiguous, so it fails construction instead of last-wins.
fn parse_additional_providers(
    value: &Value,
    primary: &Option<ProviderLane>,
) -> Result<Vec<ProviderLane>> {
    let items = value
        .as_array()
        .ok_or_else(|| errors::invalid("additionalProviders must be an array when present"))?;
    if items.len() > MAX_ADDITIONAL_PROVIDERS {
        return Err(errors::invalid(format!(
            "additionalProviders must hold at most {MAX_ADDITIONAL_PROVIDERS} entries"
        )));
    }
    let primary_id = primary
        .as_ref()
        .map_or(JS_PROVIDER_ID, |lane| lane.provider_id.as_str());
    let mut lanes: Vec<ProviderLane> = Vec::with_capacity(items.len());
    for item in items {
        let lane = parse_provider_config(item)?;
        if lane.provider_id == primary_id
            || lanes
                .iter()
                .any(|seen| seen.provider_id == lane.provider_id)
        {
            return Err(errors::invalid(format!(
                "additionalProviders providerId \"{}\" duplicates another configured provider",
                lane.provider_id
            )));
        }
        lanes.push(lane);
    }
    Ok(lanes)
}

/// Construct one configured provider lane over its registered connection.
async fn build_lane_provider(
    lane: &ProviderLane,
    fence: crate::provider_connection::Fence,
) -> Result<Arc<dyn agentkit_core::ChatProvider>> {
    let transport = crate::provider_connection::BindingTransport::connect(
        &lane.provider_id,
        lane.protocol.as_str(),
        &lane.connection_id,
        fence,
    )
    .await?;
    let preset: Arc<dyn agentkit_core::ChatProvider> = match lane.protocol {
        ProviderProtocol::ChatCompletions => Arc::new(
            agentkit_providers::ChatCompletionsProvider::new(transport).map_err(errors::core)?,
        ),
        ProviderProtocol::Responses => {
            Arc::new(agentkit_providers::ResponsesProvider::new(transport).map_err(errors::core)?)
        }
    };
    Ok(Arc::new(
        crate::provider_connection::ConfiguredProvider::new(
            preset,
            lane.temperature,
            lane.max_tokens,
        ),
    ))
}

fn parse_mode(value: &Value, key: &str) -> Result<CodingMode> {
    let text = value
        .as_str()
        .ok_or_else(|| errors::invalid(format!("{key} must be a string")))?;
    CodingMode::parse(text).ok_or_else(|| errors::invalid(format!("unknown {key} \"{text}\"")))
}

fn parse_when_busy(object: &Value) -> Result<WhenBusy> {
    match object.get("whenBusy") {
        None | Some(Value::Null) => Ok(WhenBusy::Reject),
        Some(Value::String(value)) => match value.as_str() {
            "reject" => Ok(WhenBusy::Reject),
            "queue" => Ok(WhenBusy::Queue),
            "steer" => Ok(WhenBusy::Steer),
            other => Err(errors::invalid(format!("unknown whenBusy \"{other}\""))),
        },
        Some(_) => Err(errors::invalid("whenBusy must be a string")),
    }
}

fn parse_on_settled(object: &Value, key: &str) -> Result<Option<OnSettled>> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => match value.as_str() {
            "followUp" | "follow-up" => Ok(Some(OnSettled::FollowUp)),
            "cancel" => Ok(Some(OnSettled::Cancel)),
            other => Err(errors::invalid(format!("unknown {key} \"{other}\""))),
        },
        Some(_) => Err(errors::invalid(format!("{key} must be a string"))),
    }
}

fn required_content(object: &Value) -> Result<Value> {
    object
        .get("content")
        .cloned()
        .filter(|value| !value.is_null())
        .ok_or_else(|| errors::invalid("content is required"))
}

fn parse_create_session(text: &str) -> Result<CreateSessionInput> {
    let object = parse_object(text, "the session input")?;
    let profile_id = optional_string(&object, "profileId")?
        .filter(|text| !text.is_empty())
        .ok_or_else(|| errors::invalid("profileId is required and must be a non-empty string"))?;
    let skills = match object.get("skills") {
        None => None,
        Some(Value::Array(items)) => Some(items.clone()),
        // A present non-array (null included) reaches the native reader
        // unchanged: `[null]` fails the Skills entry grammar exactly as the
        // internal path does — explicit null is malformed-present, not
        // absent, and the binding must not launder it away beforehand.
        Some(value) => Some(vec![value.clone()]),
    };
    let metadata = match object.get("metadata") {
        None | Some(Value::Null) => None,
        Some(value) if value.is_object() => Some(value.clone()),
        Some(_) => return Err(errors::invalid("metadata must be an object when present")),
    };
    // Same grammar as `setModel`: absent is "leave the profile default",
    // explicit null clears it, and an unknown effort is a refusal, never a
    // silent downgrade.
    let reasoning_effort = match object.get("reasoningEffort") {
        None => None,
        Some(Value::Null) => None,
        Some(Value::String(value)) => Some(
            serde_json::from_value::<agentkit_core::ReasoningEffort>(Value::String(value.clone()))
                .map_err(|_| errors::invalid(format!("unknown reasoningEffort \"{value}\"")))?,
        ),
        Some(_) => return Err(errors::invalid("reasoningEffort must be a string or null")),
    };
    // The selection crosses as JSON so the portable contract stays the single
    // schema: a malformed-present selection refuses here, exactly as the
    // internal path refuses it at descriptor validation time.
    let content_selection = match object.get("contentSelection") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            serde_json::from_value::<agentkit_sessions::RunContentSelection>(value.clone())
                .map_err(|error| {
                    errors::invalid(format!("contentSelection is invalid: {error}"))
                })?,
        ),
    };
    Ok(CreateSessionInput {
        session_id: optional_string(&object, "sessionId")?,
        title: optional_string(&object, "title")?,
        profile_id,
        reasoning_effort,
        content_selection,
        profile_digest: optional_string_strict(&object, "profileDigest")?,
        provider_id: optional_string_strict(&object, "providerId")?,
        model: optional_string_strict(&object, "model")?,
        workspace: object.get("workspace").cloned(),
        context: object.get("context").cloned(),
        skills,
        turn_kind: optional_string_strict(&object, "turnKind")?,
        execution_mode: optional_string(&object, "executionMode")?,
        metadata,
    })
}

fn parse_send_input(text: &str, default_busy: WhenBusy) -> Result<SendInput> {
    let object = parse_object(text, "the send input")?;
    let when_busy = match object.get("whenBusy") {
        None | Some(Value::Null) => default_busy,
        Some(_) => parse_when_busy(&object)?,
    };
    Ok(SendInput {
        content: required_content(&object)?,
        input_id: optional_string(&object, "inputId")?,
        when_busy,
        turn_id: optional_string(&object, "turnId")?,
        on_settled: parse_on_settled(&object, "onSettled")?,
    })
}

fn parse_follow_up(text: &str) -> Result<FollowUpInput> {
    let object = parse_object(text, "the follow-up input")?;
    Ok(FollowUpInput {
        content: required_content(&object)?,
        input_id: optional_string(&object, "inputId")?,
    })
}

fn parse_steer(text: &str) -> Result<SteerInput> {
    let object = parse_object(text, "the steer input")?;
    Ok(SteerInput {
        content: required_content(&object)?,
        input_id: optional_string(&object, "inputId")?,
        turn_id: optional_string(&object, "turnId")?,
        on_settled: parse_on_settled(&object, "onSettled")?,
    })
}

fn parse_answer(text: &str) -> Result<AnswerInput> {
    let object = parse_object(text, "the answer input")?;
    let request_id = optional_string(&object, "requestId")?
        .filter(|text| !text.is_empty())
        .ok_or_else(|| errors::invalid("requestId is required"))?;
    let answer_id = optional_string(&object, "answerId")?
        .filter(|text| !text.is_empty())
        .ok_or_else(|| errors::invalid("answerId is required"))?;
    let response = object
        .get("response")
        .cloned()
        .ok_or_else(|| errors::invalid("response is required"))?;
    Ok(AnswerInput {
        request_id,
        answer_id,
        response,
    })
}

fn session_state_json(state: &agentkit_sessions::AgentSessionState) -> String {
    let pending: Vec<Value> = state
        .pending
        .iter()
        .filter_map(|record| serde_json::to_value(record).ok())
        .collect();
    let waiting = state
        .waiting
        .as_ref()
        .and_then(|record| serde_json::to_value(record).ok());
    json!({
        "sessionId": state.session_id,
        "descriptor": state.descriptor,
        "activeTurn": state.active_turn,
        "pending": pending,
        "waiting": waiting,
        "waitingApproval": state.waiting_approval,
        "waitingPlan": state.waiting_plan,
        "execution": state.execution,
    })
    .to_string()
}

fn send_result_json(result: AgentSendResult) -> String {
    match result {
        AgentSendResult::Turn {
            turn_id,
            user_message_id,
            assistant_message_id,
        } => json!({
            "kind": "turn",
            "turnId": turn_id,
            "userMessageId": user_message_id,
            "assistantMessageId": assistant_message_id,
        })
        .to_string(),
        AgentSendResult::Queued { input } => {
            let record = serde_json::to_value(&*input).unwrap_or(Value::Null);
            json!({ "kind": "queued", "input": record }).to_string()
        }
    }
}

fn receipt_json(receipt: &agentkit_store::AnswerReceipt) -> String {
    json!({
        "requestId": receipt.request_id,
        "answerId": receipt.answer_id,
        "turnId": receipt.turn_id,
        "messageId": receipt.message_id,
        "duplicate": receipt.duplicate,
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn lane(max_tokens: Value) -> Result<ProviderLane> {
        parse_provider_config(&json!({
            "protocol": "responses",
            "providerId": "p",
            "connectionId": "c",
            "model": "m",
            "maxTokens": max_tokens,
        }))
    }

    fn options_with(body: Value) -> Result<ServiceOptions> {
        parse_service_options(Some(&body.to_string()))
    }

    #[test]
    fn js_lane_declares_parameter_capabilities_only_for_itself() {
        // The default JS callback lane is API-key shaped: the host stamps
        // the implicit output cap exactly as before.
        assert!(!parse_service_options(None).unwrap().account_backed_provider);
        assert!(
            !options_with(json!({"providerAuthKind": "api-key"}))
                .unwrap()
                .account_backed_provider
        );
        // An account-backed declaration suppresses the implicit cap.
        assert!(
            options_with(json!({"providerAuthKind": "chatgpt-account"}))
                .unwrap()
                .account_backed_provider
        );
        for rejected in [json!("account"), json!("API-KEY"), json!(null), json!(1)] {
            let error = options_with(json!({"providerAuthKind": rejected.clone()}))
                .err()
                .unwrap_or_else(|| panic!("{rejected} must be refused"));
            let payload: Value = serde_json::from_str(&error.reason).unwrap();
            assert_eq!(payload["code"], "invalid_input", "{rejected}");
        }
        // The Rust lane over providerConfig is always API-key shaped, so
        // declaring an account adapter alongside it is a contradiction.
        let conflicting = options_with(json!({
            "providerAuthKind": "chatgpt-account",
            "providerConfig": {
                "protocol": "responses",
                "providerId": "p",
                "connectionId": "c",
                "model": "m"
            }
        }))
        .err()
        .expect("account lane over providerConfig must be refused");
        let payload: Value = serde_json::from_str(&conflicting.reason).unwrap();
        assert_eq!(payload["code"], "invalid_input");
        // Declaring the api-key kind explicitly is consistent with it.
        assert!(
            !options_with(json!({
                "providerAuthKind": "api-key",
                "providerConfig": {
                    "protocol": "responses",
                    "providerId": "p",
                    "connectionId": "c",
                    "model": "m"
                }
            }))
            .unwrap()
            .account_backed_provider
        );
    }

    #[test]
    fn provider_config_max_tokens_is_resolved_at_construction() {
        // P1-F: service construction is the earliest layer that sees
        // `providerConfig.maxTokens`, so out-of-range values die here —
        // they never reach admission, the budget, or the wire.
        assert_eq!(lane(json!(128)).unwrap().max_tokens, Some(128));
        assert_eq!(
            lane(json!(9_007_199_254_740_991u64)).unwrap().max_tokens,
            Some(9_007_199_254_740_991)
        );
        for rejected in [
            json!(18_014_398_509_481_984_u64),
            json!(0),
            json!(-5),
            json!(1.5),
            json!("128"),
        ] {
            let error = lane(rejected.clone()).expect_err(&rejected.to_string());
            let payload: Value = serde_json::from_str(&error.reason).unwrap();
            assert_eq!(payload["code"], "invalid_input", "{rejected}");
            assert!(
                error
                    .to_string()
                    .contains("providerConfig.maxTokens must be a positive safe integer"),
                "{rejected}: {error}"
            );
        }
        // Absent or null means "no configured cap", not a rejection.
        let absent = parse_provider_config(&json!({
            "protocol": "responses",
            "providerId": "p",
            "connectionId": "c",
            "model": "m"
        }))
        .unwrap();
        assert_eq!(absent.max_tokens, None);
    }

    fn lane_json(provider_id: &str) -> Value {
        json!({
            "protocol": "chat-completions",
            "providerId": provider_id,
            "connectionId": format!("conn-{provider_id}"),
            "model": "m",
        })
    }

    fn invalid_input(error: napi::Error) -> String {
        let payload: Value = serde_json::from_str(&error.reason).unwrap();
        assert_eq!(payload["code"], "invalid_input", "{error}");
        error.to_string()
    }

    #[test]
    fn additional_providers_parse_beside_either_primary_lane() {
        let js_primary =
            options_with(json!({"additionalProviders": [lane_json("a"), lane_json("b")]})).unwrap();
        assert_eq!(js_primary.additional_providers.len(), 2);
        let configured_primary = options_with(json!({
            "providerConfig": lane_json("main"),
            "additionalProviders": [lane_json("a")],
        }))
        .unwrap();
        assert_eq!(configured_primary.additional_providers[0].provider_id, "a");
        assert!(
            options_with(json!({"additionalProviders": []}))
                .unwrap()
                .additional_providers
                .is_empty()
        );
    }

    #[test]
    fn additional_providers_refuse_duplicate_primary_oversize_and_bad_shape() {
        let duplicate =
            options_with(json!({"additionalProviders": [lane_json("a"), lane_json("a")]}))
                .err()
                .expect("duplicate id");
        assert!(invalid_input(duplicate).contains("duplicates"));
        let same_as_primary = options_with(json!({
            "providerConfig": lane_json("main"),
            "additionalProviders": [lane_json("main")],
        }))
        .err()
        .expect("primary id");
        assert!(invalid_input(same_as_primary).contains("duplicates"));
        // The JS bridge primary owns the id "js".
        let js_id = options_with(json!({"additionalProviders": [lane_json("js")]}))
            .err()
            .expect("js id");
        assert!(invalid_input(js_id).contains("duplicates"));
        let too_many: Vec<Value> = (0..17).map(|i| lane_json(&format!("p{i}"))).collect();
        let oversize = options_with(json!({"additionalProviders": too_many}))
            .err()
            .expect("17 entries");
        assert!(invalid_input(oversize).contains("at most 16"));
        for bad in [
            json!("a"),
            json!({}),
            json!([1]),
            json!([{"providerId": "a"}]),
        ] {
            let error = options_with(json!({"additionalProviders": bad.clone()}))
                .err()
                .unwrap_or_else(|| panic!("{bad} accepted"));
            invalid_input(error);
        }
    }

    #[test]
    fn declared_profiles_extend_the_registry_and_malformed_ones_fail_construction() {
        // Absent, the registry is the built-ins verbatim: the option is an
        // extension, so it cannot become a way around profile enforcement.
        assert!(parse_service_options(None).unwrap().profiles.is_empty());
        let declared = options_with(json!({"profiles": [
            {"id": "custom", "extends": "coding", "limits": {"maxToolIterations": 3}}
        ]}))
        .unwrap()
        .profiles;
        let registry =
            agentkit_sessions::ProfileRegistry::new(&declared).expect("the declared registry");
        let custom = registry.resolve("custom").expect("declared profile");
        assert_eq!(custom.limits.max_tool_iterations, Some(3));
        // `extends` inherits the parent's allow-list verbatim...
        assert_eq!(
            custom.tools,
            registry.resolve("coding").expect("built-in").tools
        );
        // ...and the built-ins stay admissible beside it, while an
        // undeclared id still has nothing to resolve against.
        assert!(registry.resolve("automation").is_ok());
        assert!(registry.resolve("undeclared").is_err());
        // The host's own validation runs at construction: a definition it
        // rejects never reaches a session that could run on a looser policy.
        let definition = |extra: Value| {
            let mut base = json!({
                "id": "case",
                "kind": "coding",
                "instructions": "",
                "tools": [],
                "permissionMode": "ask",
            });
            for (key, value) in extra.as_object().expect("entry") {
                base[key] = value.clone();
            }
            json!([base])
        };
        for rejected in [
            definition(json!({"kind": "wizard"})),
            definition(json!({"permissionMode": "sometimes"})),
            definition(json!({"tools": ["a", "a"]})),
            definition(json!({"tools": ["write_file"], "role": "reviewer"})),
            definition(json!({"limits": {"maxToolIterations": 0}})),
            json!({"id": "not-an-array"}),
        ] {
            let error = options_with(json!({"profiles": rejected.clone()}))
                .err()
                .unwrap_or_else(|| panic!("{rejected} accepted"));
            invalid_input(error);
        }
    }
}
