use super::config::{AdvisorConfig, FindingCategory, FindingSeverity, ReviewRequest};
use agentkit_core::{
    ChatMessage, ChatProvider, ChatRequest, CoreError, ExecutionBudget, RunOptions, Terminal, Tool,
    ToolLimits, ToolRegistry, run_chat,
};
use agentkit_store::{InsertFinding, SqliteStore};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use tokio_util::sync::CancellationToken;

/// Where a review came from. `Watchdog` origins pass through the loop and
/// spend guards; `Explicit` is the main agent (or host) asking directly.
#[derive(Clone, Debug)]
pub enum ReviewOrigin {
    Explicit,
    Watchdog { trigger: String },
}

impl ReviewOrigin {
    pub fn source(&self) -> &'static str {
        match self {
            Self::Explicit => "explicit",
            Self::Watchdog { .. } => "watchdog",
        }
    }
    pub fn trigger(&self) -> &str {
        match self {
            Self::Explicit => "manual",
            Self::Watchdog { trigger } => trigger,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Evidence {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u64>,
    pub detail: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct Finding {
    pub severity: FindingSeverity,
    pub category: FindingCategory,
    pub title: String,
    pub evidence: Vec<Evidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recommendation: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct AdvisorReport {
    pub run_id: String,
    pub summary: String,
    pub findings: Vec<Finding>,
    /// Durable finding ids persisted for this report (in order).
    pub finding_ids: Vec<String>,
}

/// Supplies the advisor's read-only tool registry for one review. The
/// binding implements this over the coding lane's read tools plus the LSP
/// tools; anything with a workspace effect never enters the registry, and
/// the advisor run itself carries no durable-execution authority, so a
/// write tool COULD NOT record an effect even if one slipped through.
#[async_trait::async_trait]
pub trait AdvisorToolSource: Send + Sync {
    async fn read_tools(&self, chat_id: &str) -> Vec<Arc<dyn Tool>>;
}

/// A fixed set of tools (tests, hosts without a coding lane).
pub struct StaticToolSource {
    tools: Vec<Arc<dyn Tool>>,
}

impl StaticToolSource {
    pub fn new(tools: Vec<Arc<dyn Tool>>) -> Self {
        Self { tools }
    }
}

#[async_trait::async_trait]
impl AdvisorToolSource for StaticToolSource {
    async fn read_tools(&self, _chat_id: &str) -> Vec<Arc<dyn Tool>> {
        self.tools.clone()
    }
}

/// Bridge-side event delivery for escalations (the native binding forwards
/// these to its host as `advisor.finding` events).
pub trait AdvisorEventSink: Send + Sync {
    fn emit(&self, event: Value);
}

const ADVISOR_CHAT_PREFIX: &str = "advisor:";

/// The durable id the advisor's own run logs events under. Advisor runs are
/// NEVER sessions; this id exists only so persisted findings can name the
/// run that produced them, and so the watchdog can recognize (and ignore)
/// advisor-produced traffic. The `nonce` makes every attempt unique: a
/// deterministic id would make all repeat reviews of one (chat, source,
/// trigger) replay the FIRST attempt's finding ids — later findings then
/// vanished into the insert's idempotent path and escalation bookkeeping
/// no-opped against stale rows.
pub fn advisor_run_chat_id(review_chat: &str, origin: &ReviewOrigin, nonce: &str) -> String {
    let digest = Sha256::digest(
        format!(
            "{review_chat}:{}:{}:{nonce}",
            origin.source(),
            origin.trigger()
        )
        .as_bytes(),
    );
    format!(
        "{ADVISOR_CHAT_PREFIX}{}",
        &digest
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()[..24]
    )
}

pub fn is_advisor_chat_id(chat_id: &str) -> bool {
    chat_id.starts_with(ADVISOR_CHAT_PREFIX)
}

/// A paid window slot plus the concurrency permit it will run under.
/// Dropping it without calling `review_reserved` releases the permit; the
/// slot stays spent (the attempt row is the budget denominator).
pub struct ReviewReservation {
    run_id: String,
    started_at: i64,
    permit: tokio::sync::OwnedSemaphorePermit,
}

/// Runs bounded, read-only advisory reviews with the SAME core engine
/// (`run_chat`) the session executor uses — same provider lane, same
/// budget mechanics, same cancellation — minus sessions and writes.
#[derive(Clone)]
pub struct AdvisorEngine {
    provider: Arc<dyn ChatProvider>,
    store: SqliteStore,
    runtime: Arc<dyn agentkit_core::Runtime>,
    config: Arc<Mutex<AdvisorConfig>>,
    tools: Arc<dyn AdvisorToolSource>,
    events: Option<Arc<dyn AdvisorEventSink>>,
    session: crate::AgentSessionService,
    /// Resizable at `configure` time: the permit pool is swapped to honor a
    /// hot `maxReviewConcurrency` change ("applies to the next review").
    /// In-flight holders keep the old pool until they finish.
    reviews: Arc<Mutex<Arc<tokio::sync::Semaphore>>>,
    /// Per-chat async gates making the rolling-window count + attempt-row
    /// record of `reserve` one atomic step within this process, so a burst
    /// of simultaneous triggers can never share one stale count and
    /// collectively overshoot `reviewsPerWindow`.
    budgets: Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
    model_override: Option<String>,
    /// Providers an advisor/watchdog `providerId` may name; absent = only
    /// the default provider is routable.
    providers: Option<Arc<crate::ProviderRegistry>>,
}

impl AdvisorEngine {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        provider: Arc<dyn ChatProvider>,
        store: SqliteStore,
        runtime: Arc<dyn agentkit_core::Runtime>,
        config: AdvisorConfig,
        tools: Arc<dyn AdvisorToolSource>,
        events: Option<Arc<dyn AdvisorEventSink>>,
        session: crate::AgentSessionService,
    ) -> Self {
        let config = config.effective();
        let reviews = Arc::new(tokio::sync::Semaphore::new(
            config.max_review_concurrency.max(1),
        ));
        Self {
            provider,
            store,
            runtime,
            config: Arc::new(Mutex::new(config)),
            tools,
            events,
            session,
            reviews: Arc::new(Mutex::new(reviews)),
            budgets: Arc::new(Mutex::new(HashMap::new())),
            model_override: None,
            providers: None,
        }
    }

    pub fn with_providers(mut self, providers: Arc<crate::ProviderRegistry>) -> Self {
        self.providers = Some(providers);
        self
    }

    /// The provider for this review: the config's pin for the origin, else
    /// the shared default. An unknown pin or a pin without a model fails the
    /// review loudly — never a silent fallback to the default provider.
    fn resolve_provider(
        &self,
        config: &AdvisorConfig,
        origin: &ReviewOrigin,
    ) -> crate::HostResult<Arc<dyn ChatProvider>> {
        let (pinned, model) = match origin {
            ReviewOrigin::Explicit => (&config.provider_id, &config.model),
            ReviewOrigin::Watchdog { .. } => (&config.watchdog_provider_id, &config.watchdog_model),
        };
        let Some(id) = pinned.as_deref().filter(|id| !id.is_empty()) else {
            return Ok(self.provider.clone());
        };
        if id == self.provider.id() {
            return Ok(self.provider.clone());
        }
        let provider = self
            .providers
            .as_ref()
            .and_then(|registry| registry.get(id))
            .ok_or_else(|| {
                crate::HostError::Execution(format!("advisor_provider_unavailable: {id}"))
            })?;
        if model.as_deref().is_none_or(str::is_empty) && self.model_override.is_none() {
            return Err(crate::HostError::Execution(format!(
                "advisor_model_required: provider {id} needs an explicit model"
            )));
        }
        Ok(provider)
    }

    /// Adopt a host-owned config cell instead of the engine's private one,
    /// so one write (the lane's `configureAdvisor`) is atomically visible
    /// to every advisor surface — no half-applied hot update.
    pub fn with_shared_config(mut self, config: Arc<Mutex<AdvisorConfig>>) -> Self {
        self.config = config;
        self
    }

    /// Bind a fixed model (the watchdog constructs one engine per surface
    /// with its own model), leaving the shared config for the guards.
    pub fn with_model(mut self, model: Option<String>) -> Self {
        self.model_override = model;
        self
    }

    /// The reviewed chat's durable session model, when it is a session.
    async fn session_model(&self, chat_id: &str) -> crate::HostResult<Option<String>> {
        let Some(chat) = self.store.get_chat(chat_id).await? else {
            return Ok(None);
        };
        Ok(crate::read_session_descriptor(&chat.metadata)
            .and_then(|descriptor| descriptor["model"].as_str().map(str::to_owned))
            .filter(|model| !model.is_empty()))
    }

    pub fn config(&self) -> AdvisorConfig {
        self.config
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn configure(&self, config: AdvisorConfig) {
        let config = config.effective();
        // Swap the permit pool so a hot `maxReviewConcurrency` change
        // applies to the next review instead of being silently inert.
        *self.reviews.lock().unwrap_or_else(PoisonError::into_inner) = Arc::new(
            tokio::sync::Semaphore::new(config.max_review_concurrency.max(1)),
        );
        *self.config.lock().unwrap_or_else(PoisonError::into_inner) = config;
    }

    /// Resize the review pool from an already-published (shared) config —
    /// for hosts whose lane writes the cell itself.
    pub fn sync_review_pool(&self, config: &AdvisorConfig) {
        *self.reviews.lock().unwrap_or_else(PoisonError::into_inner) = Arc::new(
            tokio::sync::Semaphore::new(config.max_review_concurrency.max(1)),
        );
    }

    pub fn store(&self) -> &SqliteStore {
        &self.store
    }

    /// One review, end to end: budget reservation → bounded read-only
    /// loop → structured parse → durable persistence → escalation of
    /// criticals.
    pub async fn review(
        &self,
        request: &ReviewRequest,
        origin: ReviewOrigin,
        cancellation: CancellationToken,
    ) -> crate::HostResult<AdvisorReport> {
        // Permit BEFORE the window slot: a saturated engine must refuse
        // without spending budget it will never use.
        let permit = self.acquire_permit()?;
        let reservation = self.reserve_gated(&request.chat_id, &origin).await?;
        self.run_review(request, origin, cancellation, reservation, permit)
            .await
    }

    fn acquire_permit(&self) -> crate::HostResult<tokio::sync::OwnedSemaphorePermit> {
        let reviews = self
            .reviews
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        reviews
            .try_acquire_owned()
            .map_err(|_| crate::HostError::Execution("advisor_saturated".into()))
    }

    /// Kill switch + loop guard (part 1: reviews never review reviews).
    fn admit(&self, chat_id: &str) -> crate::HostResult<()> {
        if !self.config().enabled {
            return Err(crate::HostError::Execution("advisor_disabled".into()));
        }
        if is_advisor_chat_id(chat_id) {
            return Err(crate::HostError::Execution(
                "advisor may not review an advisor run".into(),
            ));
        }
        Ok(())
    }

    /// Reserve this chat's next review slot: check the durable rolling-
    /// window budget and land the attempt row as ONE step under a per-chat
    /// gate. The recorded attempt is the budget denominator — a review that
    /// finds nothing still spent provider requests — and serializing
    /// count→record closes the burst overshoot where simultaneous triggers
    /// all read the same stale count. Returns the reservation (run id, start time and
    /// concurrency permit) for `review_reserved`. Budget-blocked calls consume nothing.
    pub async fn reserve(
        &self,
        chat_id: &str,
        origin: &ReviewOrigin,
    ) -> crate::HostResult<ReviewReservation> {
        self.admit(chat_id)?;
        // Permit BEFORE the window slot, carried in the reservation: a
        // saturated pool refuses without spending budget, and nothing can
        // steal the capacity between the check and the review.
        let permit = self.acquire_permit()?;
        let (run_id, started_at) = self.reserve_gated(chat_id, origin).await?;
        Ok(ReviewReservation {
            run_id,
            started_at,
            permit,
        })
    }

    /// `reserve` minus the permit, for callers that already hold theirs
    /// (`review`).
    async fn reserve_gated(
        &self,
        chat_id: &str,
        origin: &ReviewOrigin,
    ) -> crate::HostResult<(String, i64)> {
        self.admit(chat_id)?;
        let config = self.config();
        let gate = {
            let mut budgets = self.budgets.lock().unwrap_or_else(PoisonError::into_inner);
            Arc::clone(budgets.entry(chat_id.to_owned()).or_default())
        };
        let outcome = {
            let _gate = gate.lock().await;
            self.reserve_locked(chat_id, origin, &config).await
        };
        drop(gate);
        // Prune idle gates (nobody else holds the Arc) so the map does not
        // grow with every chat ever reviewed.
        self.budgets
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|_, gate| Arc::strong_count(gate) > 1);
        outcome
    }

    /// The count→record step; callers hold the chat's budget gate.
    async fn reserve_locked(
        &self,
        chat_id: &str,
        origin: &ReviewOrigin,
        config: &AdvisorConfig,
    ) -> crate::HostResult<(String, i64)> {
        // Unique per attempt (clock + process-local counter): keeps repeat
        // reviews' finding ids from replaying the first attempt's rows.
        static ATTEMPTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let started_at = self.runtime.now_ms() as i64;
        let nonce = format!(
            "{started_at}-{}",
            ATTEMPTS.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        );
        let run_id = advisor_run_chat_id(chat_id, origin, &nonce);
        // Count + record in one store job: a failing count must fail
        // CLOSED, never skip the budget.
        let reserved = self
            .store
            .reserve_advisor_review_if_under(
                &run_id,
                chat_id,
                origin.source(),
                origin.trigger(),
                started_at,
                started_at - config.window_ms,
                i64::from(config.reviews_per_window),
            )
            .await
            .map_err(|error| {
                crate::HostError::Execution(format!(
                    "advisor_rate_limited: review budget unavailable: {error}"
                ))
            })?;
        if !reserved {
            return Err(crate::HostError::Execution(format!(
                "advisor_rate_limited: {} reviews already started for this chat within {} ms",
                config.reviews_per_window, config.window_ms
            )));
        }
        Ok((run_id, started_at))
    }

    /// Run a review whose budget slot `reserve` already paid for. The
    /// watchdog pre-reserves synchronously before detaching, so a burst of
    /// triggers is bounded before any of them spawns.
    pub async fn review_reserved(
        &self,
        request: &ReviewRequest,
        origin: ReviewOrigin,
        cancellation: CancellationToken,
        reservation: ReviewReservation,
    ) -> crate::HostResult<AdvisorReport> {
        let ReviewReservation {
            run_id,
            started_at,
            permit,
        } = reservation;
        self.run_review(request, origin, cancellation, (run_id, started_at), permit)
            .await
    }

    async fn run_review(
        &self,
        request: &ReviewRequest,
        origin: ReviewOrigin,
        cancellation: CancellationToken,
        (run_id, _started_at): (String, i64),
        _permit: tokio::sync::OwnedSemaphorePermit,
    ) -> crate::HostResult<AdvisorReport> {
        let config = self.config();
        if !config.enabled {
            return Err(crate::HostError::Execution("advisor_disabled".into()));
        }
        if is_advisor_chat_id(&request.chat_id) {
            // Loop guard, part 1: reviews never review reviews.
            return Err(crate::HostError::Execution(
                "advisor may not review an advisor run".into(),
            ));
        }
        let provider = self.resolve_provider(&config, &origin)?;
        let configured = self.model_override.clone().or_else(|| match origin {
            ReviewOrigin::Explicit => config.model.clone(),
            ReviewOrigin::Watchdog { .. } => config.watchdog_model.clone(),
        });
        let model = match configured.filter(|model| !model.is_empty()) {
            Some(model) => model,
            // No advisor-specific model: review with the reviewed chat's
            // own durable session model. Never send an empty `model` — a
            // provider rejects it, failing every review opaquely.
            None => self.session_model(&request.chat_id).await?.ok_or_else(|| {
                crate::HostError::Execution(
                    "advisor_model_unconfigured: set advisor `model`/`watchdogModel` \
                         or review a session chat with a model"
                        .into(),
                )
            })?,
        };
        let tools = self.tools.read_tools(&request.chat_id).await;
        let mut registry = ToolRegistry::new();
        for tool in tools.into_iter().filter(|tool| {
            // Read-only ceiling: the advisor sees no effect-bearing tool.
            tool.definition().effect == "read"
        }) {
            registry.register(tool).ok();
        }
        let registry = Arc::new(registry);
        let system = system_prompt(&request.chat_id);
        let user = user_prompt(request);
        let request_messages = vec![
            ChatMessage::text("system", &system),
            ChatMessage::text("user", &user),
        ];
        let first = self
            .run_once(
                &provider,
                &registry,
                &model,
                request_messages.clone(),
                &config,
                &run_id,
                &cancellation,
            )
            .await?;
        let (content, mut report) = match parse_report(&first, config.max_findings) {
            Some(report) => (first, report),
            None => {
                // One bounded repair turn — a fresh invalid answer yields
                // the honest `output_invalid` finding, never a fake pass.
                let mut repair = request_messages.clone();
                repair.push(ChatMessage::text("assistant", &first));
                repair.push(ChatMessage::text(
                    "user",
                    "That answer was not a valid report. Reply with ONLY the JSON object \
                     {\"summary\": string, \"findings\": [{\"severity\": \
                     \"info|warning|critical\", \"category\": \
                     \"implementation_risk|repeated_failure|scope_deviation|regression\", \
                     \"title\": string, \"evidence\": [{\"file\"?: string, \"line\"?: number, \
                     \"detail\": string}], \"recommendation\"?: string}]}.",
                ));
                let second = self
                    .run_once(
                        &provider,
                        &registry,
                        &model,
                        repair,
                        &config,
                        &run_id,
                        &cancellation,
                    )
                    .await?;
                match parse_report(&second, config.max_findings) {
                    Some(report) => (second, report),
                    None => {
                        let head: String = second.chars().take(400).collect();
                        (
                            second,
                            AdvisorReport {
                                run_id: String::new(),
                                summary: format!(
                                    "Advisor output failed validation twice; raw answer head: {head}"
                                ),
                                findings: vec![Finding {
                                    severity: FindingSeverity::Warning,
                                    category: FindingCategory::OutputInvalid,
                                    title: "Advisor produced an unparseable report".into(),
                                    evidence: vec![Evidence {
                                        file: None,
                                        line: None,
                                        detail: head,
                                    }],
                                    recommendation: Some(
                                        "Check the advisor model's instruction following.".into(),
                                    ),
                                }],
                                finding_ids: vec![],
                            },
                        )
                    }
                }
            }
        };
        let _ = content;
        report.run_id = run_id.clone();

        // Persist every finding durably; ids are deterministic per run so
        // an at-least-once caller replaying the same run cannot duplicate.
        let mut finding_ids = Vec::with_capacity(report.findings.len());
        for (index, finding) in report.findings.iter().enumerate() {
            let finding_id = format!(
                "find_{}",
                &Sha256::digest(format!("{run_id}:{index}").as_bytes())
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>()[..24]
            );
            let detail = json!({
                "runId": run_id,
                "reportSummary": report.summary,
                "evidence": finding.evidence,
                "recommendation": finding.recommendation,
                "category": finding.category.as_str(),
            });
            self.store
                .insert_advisor_finding(InsertFinding {
                    finding_id: finding_id.clone(),
                    chat_id: request.chat_id.clone(),
                    source: origin.source().to_owned(),
                    trigger_kind: origin.trigger().to_owned(),
                    severity: finding.severity.as_str().to_owned(),
                    title: finding.title.clone(),
                    detail,
                    model: if model.is_empty() {
                        "default".into()
                    } else {
                        model.clone()
                    },
                    now: self.runtime.now_ms() as i64,
                })
                .await?;
            finding_ids.push(finding_id);
        }
        report.finding_ids = finding_ids;

        // Escalation: announce always; criticals additionally confront the
        // main agent through a durable steer input on the reviewed session.
        if let Some(sink) = &self.events {
            sink.emit(json!({
                "type": "advisor.finding",
                "chatId": request.chat_id,
                "runId": run_id,
                "source": origin.source(),
                "trigger": origin.trigger(),
                "summary": report.summary,
                "findings": report.findings,
                "findingIds": report.finding_ids,
            }));
        }
        if config.escalate_critical
            && report
                .findings
                .iter()
                .any(|finding| finding.severity == FindingSeverity::Critical)
        {
            let digest = Sha256::digest(format!("{run_id}:escalate").as_bytes());
            let request_id = format!(
                "advise_{}",
                &digest
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>()[..24]
            );
            let criticals: Vec<&Finding> = report
                .findings
                .iter()
                .filter(|finding| finding.severity == FindingSeverity::Critical)
                .collect();
            let note = format!(
                "ADVISORY (critical, from {} review {run_id}): {}",
                origin.source(),
                serde_json::to_string(&criticals).unwrap_or_else(|_| "unserializable".into())
            );
            // The note embeds model-authored text into a durable steer
            // input; cap it so an injected run cannot bloat the durable
            // store through the escalation channel.
            const MAX_STEER_NOTE_CHARS: usize = 32 * 1024;
            let note = if note.chars().count() > MAX_STEER_NOTE_CHARS {
                let head: String = note.chars().take(MAX_STEER_NOTE_CHARS).collect();
                format!("{head}\n…[truncated]")
            } else {
                note
            };
            // Durable steer: lands in the live turn at its next tool
            // boundary. NEVER on an idle session: a steer there starts a
            // new turn, and these notes embed model-authored text — a
            // prompt-injected finding would drive an unattended turn. An
            // idle session keeps the finding stored (un-escalated) and the
            // `advisor.finding` event above is its only surfacing.
            // The escalated flag is set ONLY when the steer was accepted:
            // marking it on a failed steer would claim a user-visible
            // surfacing that never happened (and the durable input id
            // means the attempt is not silently lost — the finding row
            // stays queryable and un-escalated).
            let steered = self
                .session
                .steer_live(
                    &request.chat_id,
                    &crate::SteerInput { frozen_payload: None,
                        content: Value::String(note),
                        input_id: Some(request_id),
                        turn_id: None,
                        on_settled: None,
                    },
                )
                .await
                .is_ok();
            if steered {
                for (finding, finding_id) in report.findings.iter().zip(report.finding_ids.iter()) {
                    if finding.severity == FindingSeverity::Critical {
                        let _ = self.store.mark_advisor_finding_escalated(finding_id).await;
                    }
                }
            }
        }
        Ok(report)
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_once(
        &self,
        provider: &Arc<dyn ChatProvider>,
        registry: &Arc<ToolRegistry>,
        model: &str,
        messages: Vec<ChatMessage>,
        config: &AdvisorConfig,
        run_id: &str,
        cancellation: &CancellationToken,
    ) -> crate::HostResult<String> {
        let limits = agentkit_core::BudgetLimits {
            provider_requests: config.provider_requests,
            overall_ms: config.overall_timeout_ms,
            tool_ms: Some(config.tool_timeout_ms),
            ..Default::default()
        };
        let budget = Arc::new(
            ExecutionBudget::new(limits, self.runtime.clone(), None)
                .map_err(|error| crate::HostError::Execution(format!("invalid_budget: {error}")))?,
        );
        let request = ChatRequest {
            run_id: run_id.to_owned(),
            model: model.to_owned(),
            messages,
            budget: Some(budget),
            runtime: Some(self.runtime.clone()),
            ..Default::default()
        };
        let options = RunOptions {
            request,
            run_id: Some(run_id.to_owned()),
            max_tool_iterations: config.max_iterations,
            max_tool_calls_per_iteration: config.max_tool_calls_per_iteration,
            default_tool_timeout_ms: Some(config.tool_timeout_ms),
            limits: ToolLimits::default(),
            cancellation: cancellation.clone(),
            runtime: self.runtime.clone(),
            ..Default::default()
        };
        let handle = run_chat(options, provider.clone(), registry.clone());
        // Drain the event stream to completion (backpressure on the core
        // channel), then join the result.
        futures::StreamExt::for_each(handle.events, |_event| async {}).await;
        let result = handle
            .result
            .await
            .map_err(|error| crate::HostError::Execution(format!("advisor join: {error}")))?;
        match result.terminal {
            Terminal::Completed => {}
            Terminal::Cancelled => return Err(crate::HostError::Cancelled),
            Terminal::Failed => {
                return Err(crate::HostError::Execution(
                    "advisor run failed (budget or provider)".into(),
                ));
            }
        }
        let answer = result
            .appended_messages
            .iter()
            .rev()
            .find(|message| message.role == "assistant")
            .map(|message| match &message.content {
                Value::String(text) => text.clone(),
                other => other.to_string(),
            })
            .unwrap_or_default();
        Ok(answer)
    }
}

fn system_prompt(chat_id: &str) -> String {
    format!(
        "You are AgentKit's advisor: a READ-ONLY review agent. The subject is session \
         {chat_id}. Investigate only with your read tools (files, search, git inspect, LSP). \
         Never speculate about code you have not read: every finding must cite a file and/or \
         location with concrete evidence. Judge the assignment given in the user message for \
         implementation risks, repeated-failure patterns, scope deviation from what was asked, \
         and potential regressions. Answer with ONLY one JSON object: \
         {{\"summary\": string, \"findings\": [{{\"severity\": \"info|warning|critical\", \
         \"category\": \"implementation_risk|repeated_failure|scope_deviation|regression\", \
         \"title\": string, \"evidence\": [{{\"file\"?: string, \"line\"?: number, \"detail\": \
         string}}], \"recommendation\"?: string}}]}}. No findings is a valid answer: \
         {{\"summary\": string, \"findings\": []}}."
    )
}

fn user_prompt(request: &ReviewRequest) -> String {
    let mut text = format!(
        "Review subject session: {}\nFocus: {}",
        request.chat_id, request.focus
    );
    if let Some(hint) = &request.evidence_hint {
        text.push_str(&format!("\nDurable evidence collected so far:\n{hint}"));
    }
    text
}

/// Accept fenced or bare JSON objects; reject everything that fails the
/// envelope contract (no object, no summary, no findings array). Individual
/// findings that break the evidence contract (a missing title or an unknown
/// severity/category) are DROPPED, never laundered; a report whose findings
/// are ALL malformed is rejected wholesale so the repair turn gets a shot.
fn parse_report(raw: &str, max_findings: usize) -> Option<AdvisorReport> {
    let trimmed = raw.trim();
    let body = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .and_then(|rest| rest.strip_suffix("```"))
        .unwrap_or(trimmed)
        .trim();
    let start = body.find('{')?;
    let end = body.rfind('}')?;
    if end < start {
        return None;
    }
    let value: Value = serde_json::from_str(&body[start..=end]).ok()?;
    let summary = value["summary"].as_str()?.trim().to_owned();
    if summary.is_empty() {
        return None;
    }
    let raw_findings = value["findings"].as_array()?;
    let mut findings = Vec::new();
    let mut dropped = 0usize;
    for item in raw_findings {
        let title = item["title"].as_str().unwrap_or("").trim().to_owned();
        if title.is_empty() {
            dropped += 1;
            continue;
        }
        let Some(severity) = item["severity"].as_str().and_then(FindingSeverity::parse) else {
            dropped += 1;
            continue;
        };
        let Some(category) = item["category"].as_str().and_then(FindingCategory::parse) else {
            dropped += 1;
            continue;
        };
        // The item COUNT is model-authored (like every other string here):
        // bound it, so one finding cannot persist an unbounded evidence
        // array. 32 quotes per finding is far past any legitimate report.
        const MAX_EVIDENCE_ITEMS: usize = 32;
        let evidence = item["evidence"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| {
                        let detail = item["detail"].as_str()?.trim().to_owned();
                        if detail.is_empty() {
                            return None;
                        }
                        Some(Evidence {
                            file: item["file"].as_str().map(str::to_owned),
                            line: item["line"].as_u64(),
                            detail,
                        })
                    })
                    .take(MAX_EVIDENCE_ITEMS)
                    .collect()
            })
            .unwrap_or_default();
        findings.push(Finding {
            severity,
            category,
            title,
            evidence,
            recommendation: item["recommendation"]
                .as_str()
                .filter(|text| !text.trim().is_empty())
                .map(str::to_owned),
        });
        if findings.len() >= max_findings {
            break;
        }
    }
    if findings.is_empty() && dropped > 0 {
        // Nothing survived validation: an honest failure beats a report
        // that silently "found" nothing.
        return None;
    }
    Some(AdvisorReport {
        run_id: String::new(),
        summary,
        findings,
        finding_ids: vec![],
    })
}

/// Map a core error into the host's error channel.
impl From<CoreError> for crate::HostError {
    fn from(error: CoreError) -> Self {
        crate::HostError::Execution(format!("{}: {}", error.code, error.message))
    }
}

#[cfg(test)]
mod tests {
    use super::parse_report;

    #[test]
    fn closing_brace_before_opening_brace_is_rejected_not_a_panic() {
        assert!(parse_report("} then {", 5).is_none());
        assert!(parse_report("see } and later {", 5).is_none());
    }

    #[test]
    fn bare_object_still_parses() {
        let report = parse_report(r#"{"summary":"ok","findings":[]}"#, 5).unwrap();
        assert_eq!(report.summary, "ok");
    }
}
