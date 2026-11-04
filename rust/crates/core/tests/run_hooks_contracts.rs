//! Core run-loop contract tests for the frozen hook/terminal behavior:
//! the `beforeToolCall` decision table (frozen `runToolGate`), the overall
//! execution budget bounding hook waits, and the final-settlement
//! cancellation recheck.

use agentkit_core::*;
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

struct ScriptProvider {
    calls: AtomicUsize,
}
#[async_trait]
impl ChatProvider for ScriptProvider {
    fn id(&self) -> &str {
        "script"
    }
    fn stream_chat(&self, _: ChatRequest, _: CancellationToken) -> ProviderStream {
        let first = self.calls.fetch_add(1, Ordering::SeqCst) == 0;
        Box::pin(futures_util::stream::iter([
            Ok(ProviderEvent::Started),
            Ok(if first {
                ProviderEvent::Completed {
                    content: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "call-1".into(),
                        name: "echo".into(),
                        arguments_json: "{}".into(),
                    }],
                    finish_reason: "tool_calls".into(),
                }
            } else {
                ProviderEvent::Completed {
                    content: "final".into(),
                    tool_calls: vec![],
                    finish_reason: "stop".into(),
                }
            }),
        ]))
    }
}

struct FinalProvider;
#[async_trait]
impl ChatProvider for FinalProvider {
    fn id(&self) -> &str {
        "final"
    }
    fn stream_chat(&self, _: ChatRequest, _: CancellationToken) -> ProviderStream {
        // finish_reason=tool_calls with no reconstructable call: the loop
        // synthesizes the final `tool_call_unparseable` warning.
        Box::pin(futures_util::stream::iter([
            Ok(ProviderEvent::Started),
            Ok(ProviderEvent::Completed {
                content: "half an answer".into(),
                tool_calls: vec![],
                finish_reason: "tool_calls".into(),
            }),
        ]))
    }
}

struct Echo {
    definition: ToolDefinition,
    executions: Arc<AtomicUsize>,
}
#[async_trait]
impl Tool for Echo {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }
    async fn execute(&self, context: ToolContext, _: Value) -> Result<ToolResult, CoreError> {
        self.executions.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult {
            ok: true,
            data: Some(json!({"echoed": true})),
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

fn echo_registry(executions: Arc<AtomicUsize>) -> Arc<ToolRegistry> {
    let mut registry = ToolRegistry::new();
    registry
        .register(Arc::new(Echo {
            definition: ToolDefinition {
                name: "echo".into(),
                version: "1".into(),
                effect: "read".into(),
                capability: "test".into(),
                description: "echo".into(),
                input_schema: json!({"type":"object"}),
                output_schema: None,
                timeout_ms: None,
            },
            executions,
        }))
        .unwrap();
    Arc::new(registry)
}

/// Verdict-envelope hook double: answers `name` at index 0 with a canned
/// result — the envelope the JS bridge sends (`{"value": …, "defined": …}`),
/// a raw error (threw), or a hang (deadline tests).
struct Gate {
    names: Vec<String>,
    verdict: Result<Value, ()>,
    hang: bool,
}
#[async_trait]
impl RunHooks for Gate {
    fn indexes(&self, name: &str) -> Vec<usize> {
        if self.names.iter().any(|entry| entry == name) {
            vec![0]
        } else {
            vec![]
        }
    }
    async fn call(
        &self,
        _: &str,
        _: usize,
        _: Value,
        cancellation: &CancellationToken,
    ) -> Result<Value, CoreError> {
        if self.hang {
            tokio::time::timeout(std::time::Duration::from_secs(30), cancellation.cancelled())
                .await
                .map(|_| Err(CoreError::new("cancelled", "aborted")))
                .unwrap_or(Err(CoreError::new("hook_timeout", "hung")))
        } else {
            self.verdict
                .clone()
                .map_err(|_| CoreError::new("hook_failed", "threw"))
        }
    }
}

async fn drive(mut handle: RunHandle) -> (Vec<AiRunEvent>, RunResult) {
    let mut events = vec![];
    while let Some(event) = handle.events.next().await {
        events.push(event);
    }
    let result = handle.result.await.unwrap();
    (events, result)
}

fn options() -> RunOptions {
    RunOptions {
        request: ChatRequest {
            model: "m".into(),
            messages: vec![ChatMessage::text("user", "go")],
            ..Default::default()
        },
        max_tool_iterations: 3,
        ..Default::default()
    }
}

fn value(event: &AiRunEvent) -> Value {
    serde_json::to_value(event).expect("events serialize")
}
fn kinds(events: &[AiRunEvent]) -> Vec<String> {
    events
        .iter()
        .map(|event| value(event)["type"].as_str().unwrap_or_default().to_owned())
        .collect()
}
fn denied(events: &[AiRunEvent], code: &str) -> Option<Value> {
    events
        .iter()
        .map(value)
        .find(|value| value["type"] == "run.tool.failed" && value["data"]["errorCode"] == code)
        .map(|value| value["data"].clone())
}

/// The frozen `runToolGate` table, verbatim across the bridge envelope.
#[tokio::test]
async fn before_tool_call_table_allows_only_undefined_and_explicit_allow() {
    let allow = [
        // undefined verdict — no opinion, the call proceeds.
        json!({"value": null, "defined": false}),
        json!({"value": {"action": "allow"}, "defined": true}),
        json!({"value": {"action": "allow", "reason": 7}, "defined": true}),
    ];
    for envelope in allow {
        let executions = Arc::new(AtomicUsize::new(0));
        let (events, result) = drive(run_chat(
            RunOptions {
                hooks: Some(Arc::new(Gate {
                    names: vec!["beforeToolCall".into()],
                    verdict: Ok(envelope.clone()),
                    hang: false,
                })),
                ..options()
            },
            Arc::new(ScriptProvider {
                calls: AtomicUsize::new(0),
            }),
            echo_registry(executions.clone()),
        ))
        .await;
        assert_eq!(
            executions.load(Ordering::SeqCst),
            1,
            "{envelope} must allow the call"
        );
        assert_eq!(result.terminal, Terminal::Completed);
        assert!(denied(&events, "tool_hook_denied").is_none(), "{envelope}");
    }
}

#[tokio::test]
async fn before_tool_call_table_denies_everything_unrecognized() {
    let cases: Vec<(Value, &str)> = vec![
        // null / NaN (JSON-spelled null) / booleans / numbers / strings:
        // garbage answers deny with the fixed gate sentence.
        (
            json!({"value": null, "defined": true}),
            "A loop hook could not authorize this tool call.",
        ),
        (
            json!({"value": false, "defined": true}),
            "A loop hook could not authorize this tool call.",
        ),
        (
            json!({"value": true, "defined": true}),
            "A loop hook could not authorize this tool call.",
        ),
        (
            json!({"value": 42, "defined": true}),
            "A loop hook could not authorize this tool call.",
        ),
        (
            json!({"value": "allow", "defined": true}),
            "A loop hook could not authorize this tool call.",
        ),
        (
            json!({"value": {}, "defined": true}),
            "A loop hook could not authorize this tool call.",
        ),
        (
            json!({"value": {"action": "nope"}, "defined": true}),
            "A loop hook could not authorize this tool call.",
        ),
        (
            json!({"value": {"action": "allow"}}),
            "A loop hook could not authorize this tool call.",
        ),
        // deny carries its reason only when it is a plain string.
        (
            json!({"value": {"action": "deny", "reason": "policy"}, "defined": true}),
            "policy",
        ),
        (
            json!({"value": {"action": "deny"}, "defined": true}),
            "A loop hook could not authorize this tool call.",
        ),
        (
            json!({"value": {"action": "deny", "reason": 7}, "defined": true}),
            "A loop hook could not authorize this tool call.",
        ),
        // An envelope missing the flag entirely is unreadable: deny.
        (
            json!({"value": {"action": "allow"}}),
            "A loop hook could not authorize this tool call.",
        ),
    ];
    // fix the bogus extra row above ({"action":"allow"} without defined)
    for (envelope, reason) in cases {
        let expects_allow = envelope["value"]["action"] == "allow"
            && envelope.get("defined").and_then(Value::as_bool) == Some(true);
        if expects_allow {
            continue; // covered by the allow test
        }
        let executions = Arc::new(AtomicUsize::new(0));
        let (events, result) = drive(run_chat(
            RunOptions {
                hooks: Some(Arc::new(Gate {
                    names: vec!["beforeToolCall".into()],
                    verdict: Ok(envelope.clone()),
                    hang: false,
                })),
                ..options()
            },
            Arc::new(ScriptProvider {
                calls: AtomicUsize::new(0),
            }),
            echo_registry(executions.clone()),
        ))
        .await;
        assert_eq!(
            executions.load(Ordering::SeqCst),
            0,
            "{envelope} must deny the call"
        );
        let data = denied(&events, "tool_hook_denied")
            .unwrap_or_else(|| panic!("{envelope} must emit tool_hook_denied"));
        assert_eq!(data["errorMessage"], reason, "{envelope}");
        // The run continues: the model reacts to the denial and completes.
        assert_eq!(result.terminal, Terminal::Completed);
    }
}

#[tokio::test]
async fn throwing_and_timing_out_gates_fail_closed() {
    // threw → deny with the gate sentence.
    let executions = Arc::new(AtomicUsize::new(0));
    let (events, _) = drive(run_chat(
        RunOptions {
            hooks: Some(Arc::new(Gate {
                names: vec!["beforeToolCall".into()],
                verdict: Err(()),
                hang: false,
            })),
            ..options()
        },
        Arc::new(ScriptProvider {
            calls: AtomicUsize::new(0),
        }),
        echo_registry(executions.clone()),
    ))
    .await;
    assert_eq!(executions.load(Ordering::SeqCst), 0);
    assert!(denied(&events, "tool_hook_denied").is_some());

    // per-hook timeout (the smaller of the two bounds wins here: the
    // budget's overall time is huge) → deny with the gate sentence.
    let executions = Arc::new(AtomicUsize::new(0));
    let started = Instant::now();
    let (events, result) = drive(run_chat(
        RunOptions {
            hook_timeout_ms: Some(20),
            hooks: Some(Arc::new(Gate {
                names: vec!["beforeToolCall".into()],
                verdict: Ok(json!({"defined": false})),
                hang: true,
            })),
            ..options()
        },
        Arc::new(ScriptProvider {
            calls: AtomicUsize::new(0),
        }),
        echo_registry(executions.clone()),
    ))
    .await;
    assert!(
        started.elapsed().as_secs() < 3,
        "the per-hook deadline must abandon the hung call"
    );
    assert_eq!(executions.load(Ordering::SeqCst), 0);
    let data = denied(&events, "tool_hook_denied").expect("hung gate denies");
    assert_eq!(
        data["errorMessage"],
        "A loop hook could not authorize this tool call."
    );
    assert_eq!(result.terminal, Terminal::Completed);
}

#[tokio::test]
async fn overall_budget_bounds_hook_waits_and_settles_deadline_exceeded() {
    // Per-hook deadline DISABLED (0): only the run's overall execution
    // budget can end a hung gate — the frozen lane aborts with
    // `execution_timeout: overall` at the deadline, never after the hook.
    let budget = Arc::new(
        ExecutionBudget::new(
            BudgetLimits {
                overall_ms: Some(30),
                ..Default::default()
            },
            Arc::new(SystemRuntime),
            None,
        )
        .unwrap(),
    );
    let executions = Arc::new(AtomicUsize::new(0));
    let started = Instant::now();
    let (events, result) = drive(run_chat(
        RunOptions {
            hook_timeout_ms: Some(0),
            request: ChatRequest {
                budget: Some(budget.clone()),
                ..options().request
            },
            hooks: Some(Arc::new(Gate {
                names: vec!["beforeToolCall".into()],
                verdict: Ok(json!({"defined": false})),
                hang: true,
            })),
            ..options()
        },
        Arc::new(ScriptProvider {
            calls: AtomicUsize::new(0),
        }),
        echo_registry(executions.clone()),
    ))
    .await;
    let elapsed = started.elapsed();
    assert!(
        elapsed.as_millis() < 3000,
        "the hook must be abandoned at ~the overall deadline, took {elapsed:?}"
    );
    assert_eq!(executions.load(Ordering::SeqCst), 0, "the tool never runs");
    assert_eq!(result.terminal, Terminal::Failed);
    let last = value(events.last().expect("terminal event"));
    assert_eq!(last["type"], "run.failed");
    assert_eq!(last["data"]["errorCode"], "execution_timeout");
    assert_eq!(last["data"]["errorMessage"], "execution_timeout: overall");
}

#[tokio::test]
async fn overall_budget_bounds_observer_waits_silently() {
    // An observer abandoned by the overall deadline is the frozen abort
    // race: no `hook_timeout` warning; the run settles deadline-exceeded.
    let budget = Arc::new(
        ExecutionBudget::new(
            BudgetLimits {
                overall_ms: Some(30),
                ..Default::default()
            },
            Arc::new(SystemRuntime),
            None,
        )
        .unwrap(),
    );
    let (events, result) = drive(run_chat(
        RunOptions {
            hook_timeout_ms: Some(0),
            request: ChatRequest {
                budget: Some(budget.clone()),
                ..options().request
            },
            hooks: Some(Arc::new(Gate {
                names: vec!["onStepStart".into()],
                verdict: Ok(json!({"defined": false})),
                hang: true,
            })),
            ..options()
        },
        Arc::new(ScriptProvider {
            calls: AtomicUsize::new(0),
        }),
        echo_registry(Arc::new(AtomicUsize::new(0))),
    ))
    .await;
    assert_eq!(result.terminal, Terminal::Failed);
    let kinds = kinds(&events);
    assert!(
        !events.iter().map(value).any(|value| {
            value["type"] == "run.warning" && value["data"]["code"] == "hook_timeout"
        }),
        "the overall deadline abandons observers without a hook_timeout warning: {kinds:?}"
    );
    let last = value(events.last().unwrap());
    assert_eq!(last["type"], "run.failed");
    assert_eq!(last["data"]["errorCode"], "execution_timeout");
}

/// A lockstep gate: every emit parks at the event boundary until the test
/// — having already RECEIVED that event from the channel — releases one
/// permit. That makes "abort lands exactly while the final warning is being
/// synthesized" deterministic instead of racy.
struct StepGate {
    permits: tokio::sync::Semaphore,
}
#[async_trait]
impl RunEventGate for StepGate {
    async fn wait(&self, cancellation: &CancellationToken) {
        tokio::select! {
            permit = self.permits.acquire() => {
                if let Ok(permit) = permit {
                    permit.forget();
                }
            }
            _ = cancellation.cancelled() => {}
        }
    }
}

#[tokio::test]
async fn abort_during_final_warning_synthesis_cancels_without_assistant_partial() {
    let cancellation = CancellationToken::new();
    let gate = Arc::new(StepGate {
        permits: tokio::sync::Semaphore::new(0),
    });
    let handle = run_chat(
        RunOptions {
            cancellation: cancellation.clone(),
            event_gate: Some(gate.clone()),
            ..options()
        },
        Arc::new(FinalProvider),
        echo_registry(Arc::new(AtomicUsize::new(0))),
    );
    let mut events = vec![];
    let mut cancelled = false;
    let mut saw_final_warning = false;
    let mut handle = handle;
    while let Some(event) = handle.events.next().await {
        let snapshot = value(&event);
        let is_warning = snapshot["type"] == "run.warning"
            && snapshot["data"]["code"] == "tool_call_unparseable";
        if is_warning && !cancelled {
            // The abort lands while the final warning emit is parked in the
            // event gate — precisely between the last event and the commit.
            saw_final_warning = true;
            cancelled = true;
            cancellation.cancel();
        }
        events.push(event);
        gate.permits.add_permits(1);
    }
    let result = handle.result.await.unwrap();
    assert!(saw_final_warning, "the final warning path must have run");
    assert_eq!(result.terminal, Terminal::Cancelled);
    assert_eq!(
        result.appended_messages.len(),
        0,
        "a cancelled run appends no assistant partial: {:?}",
        result.appended_messages
    );
    let kinds = kinds(&events);
    assert_eq!(
        kinds.last().map(String::as_str),
        Some("run.cancelled"),
        "{kinds:?}"
    );
    assert!(
        !kinds.contains(&"run.completed".to_owned()),
        "no completed terminal after the abort: {kinds:?}"
    );
}
