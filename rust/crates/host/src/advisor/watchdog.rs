use super::config::{AdvisorConfig, ReviewRequest};
use super::engine::{AdvisorEngine, ReviewOrigin, is_advisor_chat_id};
use crate::{CHAT_TURN_TASK_KIND, HostError, HostResult, SharedRuntime, TaskSettledObserver};
use agentkit_core::{RunHooks, ToolCall};
use agentkit_store::{SqliteStore, TaskRecord, TaskStatus};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};
use tokio_util::sync::CancellationToken;

/// Signals the watchdog reacts to. Kept explicit so every trigger is an
/// auditable decision, not an ad-hoc string match.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WatchdogSignals {
    /// Same tool failing back-to-back (or the same error code repeating).
    RepeatedFailures,
    /// A turn touching more distinct files than the configured scope bound.
    ScopeDeviation,
    /// Test/command failure after edits in the same run.
    PotentialRegression,
    /// Consecutive failed turns of one session.
    RepeatedTurnFailures,
}

impl WatchdogSignals {
    pub fn trigger(self) -> &'static str {
        match self {
            Self::RepeatedFailures => "repeated_failures",
            Self::ScopeDeviation => "scope_deviation",
            Self::PotentialRegression => "potential_regression",
            Self::RepeatedTurnFailures => "repeated_turn_failures",
        }
    }
}

#[derive(Default, Clone)]
struct RunState {
    consecutive_tool_failures: u32,
    error_codes: HashMap<String, u32>,
    edited_files: HashSet<String>,
    /// Edits happened and a test/command then failed afterwards.
    regression_signal: bool,
    /// Triggers already fired for this run (one review per run+trigger).
    fired: HashSet<&'static str>,
}

#[derive(Default)]
struct SessionState {
    consecutive_failed_turns: u32,
    last_review: HashMap<&'static str, i64>,
    /// Last observation; entries idle past the retain window are pruned so
    /// the map cannot grow with lifetime chat count.
    last_seen: i64,
}

/// Minimum lifetime for a session counter. Streaks and cooldowns only
/// matter while a chat is active; the durable rolling-window budget that
/// actually bounds model spend lives in the store, so dropping counters
/// for chats idle longer than every trigger window is a safe degradation.
const SESSION_RETAIN_FLOOR_MS: i64 = 3_600_000;

fn prune_sessions(sessions: &mut HashMap<String, SessionState>, now: i64, retain_ms: i64) {
    sessions.retain(|_, state| now - state.last_seen <= retain_ms);
}

/// Event-triggered reviewer over the durable lifecycle stream. It observes
/// tool results via core run-hooks and turn settlements via the host's
/// settled observers, and fires bounded advisor reviews — never inline in
/// the reviewed run, never on advisor traffic (loop guard).
#[derive(Clone)]
pub struct Watchdog {
    inner: Arc<WatchdogInner>,
}

struct WatchdogInner {
    engine: AdvisorEngine,
    config: Arc<Mutex<AdvisorConfig>>,
    runtime: SharedRuntime,
    runs: Mutex<HashMap<String, RunState>>,
    sessions: Mutex<HashMap<String, SessionState>>,
}

impl Watchdog {
    pub fn new(
        engine: AdvisorEngine,
        config: Arc<Mutex<AdvisorConfig>>,
        runtime: SharedRuntime,
    ) -> Self {
        Self {
            inner: Arc::new(WatchdogInner {
                engine,
                config,
                runtime,
                runs: Mutex::new(HashMap::new()),
                sessions: Mutex::new(HashMap::new()),
            }),
        }
    }

    pub fn config(&self) -> AdvisorConfig {
        self.inner
            .config
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn engine(&self) -> &AdvisorEngine {
        &self.inner.engine
    }

    fn now(&self) -> i64 {
        self.inner.runtime.now_ms()
    }

    fn update<T>(&self, run_id: &str, f: impl FnOnce(&mut RunState) -> T) -> T {
        let mut runs = self
            .inner
            .runs
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let state = runs.entry(run_id.to_owned()).or_default();
        f(state)
    }

    /// Touch one session's counters under the shared lock, sweeping entries
    /// idle past the retain window in the same pass — the guard against
    /// per-chat growth over a long-lived host's lifetime.
    fn with_session<T>(
        &self,
        chat_id: &str,
        now: i64,
        config: &AdvisorConfig,
        f: impl FnOnce(&mut SessionState) -> T,
    ) -> T {
        let retain_ms = config
            .window_ms
            .max(config.cooldown_ms)
            .max(SESSION_RETAIN_FLOOR_MS);
        let mut sessions = self
            .inner
            .sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let state = sessions.entry(chat_id.to_owned()).or_default();
        state.last_seen = now;
        let out = f(state);
        prune_sessions(&mut sessions, now, retain_ms);
        out
    }

    /// One tool result, as reported by the core run loop's `onToolResult`
    /// hook. Cheap and synchronous: counters only — reviews spawn detached
    /// so the reviewed run never waits on the advisor.
    pub fn observe_tool_result(
        &self,
        chat_id: &str,
        run_id: &str,
        tool_name: &str,
        status: &str,
        error_code: Option<&str>,
    ) {
        if !self.config().enabled || is_advisor_chat_id(chat_id) {
            return;
        }
        let config = self.config();
        // Core reports tool outcomes as exactly "succeeded" | "failed"
        // (`core/run.rs`); anything else is treated as a failure signal.
        // Successes reset the streak — a comparison against invented
        // statuses here once counted every SUCCESS as a failure and fired
        // model-paying reviews on healthy runs.
        let failed = status != "succeeded";
        let mut triggers: Vec<WatchdogSignals> = vec![];
        self.update(run_id, |state| {
            if failed {
                state.consecutive_tool_failures += 1;
                let code = error_code.unwrap_or(tool_name).to_owned();
                let count = state.error_codes.entry(code).or_insert(0);
                *count += 1;
                if state.consecutive_tool_failures >= config.trigger_consecutive_tool_failures {
                    triggers.push(WatchdogSignals::RepeatedFailures);
                }
                if state
                    .error_codes
                    .values()
                    .any(|count| *count >= config.trigger_repeated_error_code)
                {
                    triggers.push(WatchdogSignals::RepeatedFailures);
                }
                if state.regression_signal {
                    triggers.push(WatchdogSignals::PotentialRegression);
                }
                if state.edited_files.len() as u32 >= config.trigger_edit_count {
                    triggers.push(WatchdogSignals::ScopeDeviation);
                }
                if (tool_name == "run_tests" || tool_name == "run_command")
                    && !state.edited_files.is_empty()
                {
                    state.regression_signal = true;
                }
            } else {
                state.consecutive_tool_failures = 0;
            }
        });
        for trigger in triggers {
            self.maybe_fire(chat_id, run_id, trigger);
        }
    }

    /// The model's declared calls for one step (arguments visible only
    /// here): records which files this run is touching, for the
    /// scope-deviation and regression signals.
    pub fn observe_tool_calls(&self, chat_id: &str, run_id: &str, calls: &[ToolCall]) {
        if !self.config().enabled || is_advisor_chat_id(chat_id) {
            return;
        }
        let threshold = self.config().trigger_edit_count;
        let mut crossed = false;
        for call in calls {
            if !matches!(call.name.as_str(), "edit_file" | "write_file") {
                continue;
            }
            let Ok(args) = serde_json::from_str::<Value>(&call.arguments_json) else {
                continue;
            };
            if let Some(path) = ["path", "file", "filePath"]
                .iter()
                .find_map(|key| args[*key].as_str())
            {
                let len = self.update(run_id, |state| {
                    state.edited_files.insert(path.to_owned());
                    state.edited_files.len()
                });
                crossed |= len as u32 >= threshold;
            }
        }
        if crossed {
            self.maybe_fire(chat_id, run_id, WatchdogSignals::ScopeDeviation);
        }
    }

    /// A task settled: track consecutive failed turns per session.
    pub fn observe_turn_settled(&self, task: &TaskRecord) {
        if !self.config().enabled || is_advisor_chat_id(&task.scope_id) {
            return;
        }
        if task.kind != CHAT_TURN_TASK_KIND {
            return;
        }
        let config = self.config();
        let now = self.now();
        let streak = self.with_session(&task.scope_id, now, &config, |state| match task.status {
            TaskStatus::Failed => {
                state.consecutive_failed_turns += 1;
                state.consecutive_failed_turns
            }
            TaskStatus::Completed => {
                state.consecutive_failed_turns = 0;
                0
            }
            _ => state.consecutive_failed_turns,
        });
        if matches!(task.status, TaskStatus::Failed) && streak >= config.trigger_turn_failures {
            self.maybe_fire(
                &task.scope_id,
                &task.task_id,
                WatchdogSignals::RepeatedTurnFailures,
            );
        }
    }

    /// Drop per-run memory once a run is done (settled tasks stop producing
    /// hooks; the next turn mints a fresh run id).
    pub fn forget_run(&self, run_id: &str) {
        self.inner
            .runs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(run_id);
    }

    fn maybe_fire(&self, chat_id: &str, run_id: &str, trigger: WatchdogSignals) {
        let now = self.now();
        let config = self.config();
        let trigger_name = trigger.trigger();
        {
            // One fire per (run, trigger), then a per-chat+trigger cooldown
            // — both halves of the anti-loop guard, decided in one lock.
            let mut fired = false;
            {
                let runs = self
                    .inner
                    .runs
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                if let Some(state) = runs.get(run_id) {
                    fired = state.fired.contains(trigger_name);
                }
            }
            if fired {
                return;
            }
            let newly_fired = self.update(run_id, |state| state.fired.insert(trigger_name));
            let allowed = self.with_session(chat_id, now, &config, |state| {
                if state
                    .last_review
                    .get(trigger_name)
                    .is_some_and(|at| now - *at < config.cooldown_ms)
                {
                    false
                } else {
                    state.last_review.insert(trigger_name, now);
                    true
                }
            });
            if !allowed {
                if newly_fired {
                    self.update(run_id, |state| {
                        state.fired.remove(trigger_name);
                    });
                }
                return;
            }
            let engine = self.inner.engine.clone();
            let store = engine.store().clone();
            let watchdog = self.clone();
            let chat = chat_id.to_owned();
            let run = run_id.to_owned();
            let origin = ReviewOrigin::Watchdog {
                trigger: trigger_name.to_owned(),
            };
            // Detached review: the reviewed run never waits on the advisor.
            tokio::spawn(async move {
                // Durable rolling-window budget (loop guard, part 2): the
                // per-chat reservation (count + attempt row, one atomic
                // step) decides the budget BEFORE the fire slots are
                // finally consumed — simultaneous triggers can never
                // share a stale count, and a budget-blocked review gives
                // its run+trigger slot back instead of burning it.
                let reservation = match engine.reserve(&chat, &origin).await {
                    Ok(reservation) => reservation,
                    Err(_) => {
                        watchdog.retract_fire(&run, &chat, trigger_name, now);
                        return;
                    }
                };
                let evidence = collect_evidence(&store, &chat, &run, trigger).await;
                let request = ReviewRequest {
                    chat_id: chat.clone(),
                    focus: format!(
                        "Automatic watchdog review ({trigger_name}): the session is exhibiting \
                         the pattern named in the trigger. Verify against the repository whether \
                         the work is at risk, out of scope, or regressed; every finding must \
                         cite evidence."
                    ),
                    evidence_hint: Some(evidence),
                };
                if let Err(error) = engine
                    .review_reserved(&request, origin, CancellationToken::new(), reservation)
                    .await
                {
                    // Code prefix only: the full text may embed provider or
                    // finding payloads.
                    let text = error.to_string();
                    let code = text.split(':').next().unwrap_or_default();
                    eprintln!("agentkit-host: watchdog review for {trigger_name} failed: {code}");
                    // These refusals land before any provider request, so the
                    // trigger may fire again once the cause is fixed.
                    if matches!(code, "advisor_disabled" | "advisor_model_unconfigured") {
                        watchdog.retract_fire(&run, &chat, trigger_name, now);
                    }
                }
            });
        }
    }

    /// Undo a fire whose budget reservation was refused: the trigger may
    /// fire later, once the window has room.
    fn retract_fire(&self, run_id: &str, chat_id: &str, trigger: &'static str, at: i64) {
        self.update(run_id, |state| {
            state.fired.remove(trigger);
        });
        let config = self.config();
        self.with_session(chat_id, self.now(), &config, |state| {
            if state.last_review.get(trigger) == Some(&at) {
                state.last_review.remove(trigger);
            }
            true
        });
    }
}

async fn collect_evidence(
    store: &SqliteStore,
    chat_id: &str,
    run_id: &str,
    trigger: WatchdogSignals,
) -> Value {
    let mut evidence = json!({"trigger": trigger.trigger(), "runId": run_id});
    if matches!(
        trigger,
        WatchdogSignals::RepeatedFailures
            | WatchdogSignals::PotentialRegression
            | WatchdogSignals::RepeatedTurnFailures
    ) {
        // The durable message log is the evidence source: the recent tool
        // results of this run, with their recorded error payloads.
        if let Ok(records) = store.messages(chat_id, true).await {
            let tail: Vec<Value> = records
                .iter()
                .filter(|record| record.role == "tool" && record.run_id.as_deref() == Some(run_id))
                .rev()
                .take(12)
                .map(|record| {
                    json!({
                        "content": record.content,
                        "metadata": record.metadata,
                    })
                })
                .collect();
            evidence["recentToolResults"] = Value::Array(tail);
        }
    }
    evidence
}

/// Host-side run-hooks adapter: core hooks know only the run id (which for
/// session turns IS the task id, whose scope is the chat), so the adapter
/// resolves chat identity once per run from the durable task record and
/// caches it.
pub struct WatchdogRunHooks {
    watchdog: Watchdog,
    store: SqliteStore,
    runs: Mutex<HashMap<String, String>>,
}

impl WatchdogRunHooks {
    pub fn new(watchdog: Watchdog, store: SqliteStore) -> Self {
        Self {
            watchdog,
            store,
            runs: Mutex::new(HashMap::new()),
        }
    }

    async fn chat_for_run(&self, run_id: &str) -> Option<String> {
        if let Some(chat) = self
            .runs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(run_id)
            .cloned()
        {
            return Some(chat);
        }
        let chat = self
            .store
            .get_task(run_id)
            .await
            .ok()
            .flatten()
            .map(|task| task.scope_id)?;
        self.runs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(run_id.to_owned(), chat.clone());
        Some(chat)
    }

    /// Drop a settled run's identity mapping: the cache must not keep one
    /// entry per turn for the host's whole lifetime.
    pub fn forget_run(&self, run_id: &str) {
        let mut runs = self.runs.lock().unwrap_or_else(PoisonError::into_inner);
        runs.remove(run_id);
        // Safety bound for pathological streams (runs that never settle):
        // drop the whole identity cache rather than grow without bound —
        // the next hook call just re-resolves from the durable task.
        if runs.len() > 8192 {
            runs.clear();
        }
    }
}

#[async_trait::async_trait]
impl RunHooks for WatchdogRunHooks {
    fn indexes(&self, name: &str) -> Vec<usize> {
        match name {
            "onToolResult" | "onModelResponse" => vec![0],
            _ => vec![],
        }
    }

    async fn call(
        &self,
        name: &str,
        _index: usize,
        context: Value,
        _cancellation: &CancellationToken,
    ) -> Result<Value, agentkit_core::CoreError> {
        let Some(run_id) = context["runId"].as_str() else {
            return Ok(Value::Null);
        };
        match name {
            "onToolResult" => {
                let Some(chat_id) = self.chat_for_run(run_id).await else {
                    return Ok(Value::Null);
                };
                self.watchdog.observe_tool_result(
                    &chat_id,
                    run_id,
                    context["toolName"].as_str().unwrap_or_default(),
                    context["status"].as_str().unwrap_or_default(),
                    context["errorCode"].as_str(),
                );
            }
            "onModelResponse" => {
                let Some(chat_id) = self.chat_for_run(run_id).await else {
                    return Ok(Value::Null);
                };
                let calls: Vec<ToolCall> = context["toolCalls"]
                    .as_array()
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(|item| {
                                Some(ToolCall {
                                    id: item["id"].as_str()?.to_owned(),
                                    name: item["name"].as_str()?.to_owned(),
                                    arguments_json: item["argumentsJson"]
                                        .as_str()
                                        .unwrap_or("{}")
                                        .to_owned(),
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                self.watchdog.observe_tool_calls(&chat_id, run_id, &calls);
            }
            _ => {}
        }
        Ok(Value::Null)
    }
}

/// Host-side observer adapter: settlements feed the turn-failure counter
/// and release per-run memory.
#[async_trait::async_trait]
impl TaskSettledObserver for Watchdog {
    async fn observe_settled(&self, task: &TaskRecord) -> HostResult<()> {
        self.observe_turn_settled(task);
        self.forget_run(&task.task_id);
        Ok(())
    }
}

impl From<WatchdogSignals> for String {
    fn from(value: WatchdogSignals) -> Self {
        value.trigger().to_owned()
    }
}

/// Convenience: explicit reviews requested by the main agent run straight
/// through the engine, sharing the watchdog's guards only for output size.
pub async fn explicit_review(
    engine: &AdvisorEngine,
    chat_id: &str,
    focus: String,
    cancellation: CancellationToken,
) -> HostResult<super::engine::AdvisorReport> {
    if chat_id.is_empty() {
        return Err(HostError::Execution("chatId is required".into()));
    }
    engine
        .review(
            &ReviewRequest {
                chat_id: chat_id.to_owned(),
                focus,
                evidence_hint: None,
            },
            ReviewOrigin::Explicit,
            cancellation,
        )
        .await
}

#[cfg(test)]
mod tests {
    use super::{SessionState, prune_sessions};
    use std::collections::HashMap;

    #[test]
    fn prune_sessions_drops_only_entries_idle_past_the_retain_window() {
        let now = 10_000_000i64;
        let retain = 3_600_000i64;
        let mut sessions: HashMap<String, SessionState> = HashMap::new();
        for (id, seen) in [
            ("fresh", now),
            ("edge", now - retain),
            ("idle", now - retain - 1),
        ] {
            sessions.insert(
                id.to_owned(),
                SessionState {
                    last_seen: seen,
                    ..Default::default()
                },
            );
        }
        prune_sessions(&mut sessions, now, retain);
        assert!(sessions.contains_key("fresh"), "active chats must survive");
        assert!(
            sessions.contains_key("edge"),
            "exactly-at-window entries are still observable"
        );
        assert!(!sessions.contains_key("idle"), "idle chats must not leak");
    }

    #[test]
    fn never_touched_entries_are_pruned() {
        // Default last_seen is 0 (never observed): a map seeded outside any
        // observation pass cannot pin memory.
        let mut sessions: HashMap<String, SessionState> = HashMap::new();
        sessions.insert("ghost".to_owned(), SessionState::default());
        prune_sessions(&mut sessions, 10_000_000, 3_600_000);
        assert!(sessions.is_empty());
    }
}
