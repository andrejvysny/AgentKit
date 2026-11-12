use agentkit_core::*;
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};

struct Clock(AtomicU64);
impl Runtime for Clock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst).cast_signed()
    }
    fn timestamp(&self) -> String {
        "2026-11-06T12:00:00.000Z".into()
    }
    fn id(&self) -> String {
        uuid::Uuid::new_v4().to_string()
    }
    fn id_with_prefix(&self, prefix: &str) -> String {
        format!("{prefix}{}", uuid::Uuid::new_v4())
    }
}

/// A provider event, or a clock advance before the next one is pulled.
enum Step {
    At(u64),
    Event(ProviderEvent),
}
struct Script {
    clock: Arc<Clock>,
    turns: Mutex<Vec<Vec<Step>>>,
}
#[async_trait]
impl ChatProvider for Script {
    fn id(&self) -> &str {
        "script"
    }
    fn stream_chat(&self, _: ChatRequest, _: CancellationToken) -> ProviderStream {
        let clock = self.clock.clone();
        let steps = self.turns.lock().unwrap().remove(0);
        Box::pin(futures_util::stream::iter(steps).filter_map(move |step| {
            let clock = clock.clone();
            async move {
                match step {
                    Step::At(ms) => {
                        clock.0.store(ms, Ordering::SeqCst);
                        None
                    }
                    Step::Event(event) => Some(Ok(event)),
                }
            }
        }))
    }
}
struct Echo(ToolDefinition);
#[async_trait]
impl Tool for Echo {
    fn definition(&self) -> &ToolDefinition {
        &self.0
    }
    async fn execute(&self, context: ToolContext, _: Value) -> Result<ToolResult, CoreError> {
        Ok(ToolResult {
            ok: true,
            data: Some(json!({"ok":true})),
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
fn event(step: ProviderEvent) -> Step {
    Step::Event(step)
}
fn completed(content: &str, calls: &[ToolCall]) -> Step {
    event(ProviderEvent::Event {
        kind: "run.message.completed".into(),
        data: json!({"content":content,"toolCallCount":calls.len(),"toolCalls":calls}),
    })
}
fn progress(bytes: u64) -> Step {
    event(ProviderEvent::ToolCallProgress {
        id: "c1".into(),
        name: "echo".into(),
        args_bytes: bytes,
    })
}
async fn run(turns: Vec<Vec<Step>>) -> (Vec<Value>, RunResult) {
    let clock = Arc::new(Clock(AtomicU64::new(0)));
    let mut registry = ToolRegistry::new();
    registry
        .register(Arc::new(Echo(
            serde_json::from_value(json!({"name":"echo","version":"1","effect":"read",
                "capability":"test","description":"Echo.","inputSchema":{"type":"object"}}))
            .unwrap(),
        )))
        .unwrap();
    let mut options = RunOptions {
        runtime: clock.clone(),
        ..Default::default()
    };
    options.request.model = "m".into();
    options.request.messages = vec![ChatMessage::text("user", "hi")];
    let handle = run_chat(
        options,
        Arc::new(Script {
            clock,
            turns: Mutex::new(turns),
        }),
        Arc::new(registry),
    );
    let events = handle
        .events
        .map(|event| serde_json::to_value(event).unwrap())
        .collect::<Vec<_>>()
        .await;
    (events, handle.result.await.unwrap())
}
fn kinds(events: &[Value]) -> Vec<&str> {
    events
        .iter()
        .map(|event| event["type"].as_str().unwrap())
        .collect()
}
fn data_of<'a>(events: &'a [Value], kind: &str) -> Vec<&'a Value> {
    events
        .iter()
        .filter(|event| event["type"] == kind)
        .map(|event| &event["data"])
        .collect()
}

#[tokio::test]
async fn tool_preparing_precedes_the_request_and_is_throttled_per_call() {
    let call = ToolCall {
        id: "c1".into(),
        name: "echo".into(),
        arguments_json: "{}".into(),
    };
    let (events, result) = run(vec![
        vec![
            event(ProviderEvent::Started),
            progress(0),
            Step::At(500),
            progress(10),
            Step::At(2_100),
            progress(20),
            Step::At(4_500),
            progress(20),
            Step::At(4_600),
            progress(30),
            completed("", std::slice::from_ref(&call)),
            event(ProviderEvent::ToolRequested(call.clone())),
            Step::At(9_000),
            progress(40),
        ],
        vec![
            event(ProviderEvent::Delta("done".into())),
            completed("done", &[]),
        ],
    ])
    .await;
    assert_eq!(result.terminal, Terminal::Completed);
    assert_eq!(
        kinds(&events),
        [
            "run.started",
            "run.tool.preparing",
            "run.tool.preparing",
            "run.tool.preparing",
            "run.message.completed",
            "run.tool.requested",
            "run.tool.running",
            "run.tool.succeeded",
            "run.message.delta",
            "run.message.completed",
            "run.completed",
        ]
    );
    assert_eq!(
        data_of(&events, "run.tool.preparing"),
        [0, 20, 30]
            .iter()
            .map(|bytes| json!({"toolCallId":"c1","toolName":"echo","argsBytes":bytes,"step":1}))
            .collect::<Vec<_>>()
            .iter()
            .collect::<Vec<_>>()
    );
    // Run events only: the conversation holds the call, its result and the answer.
    let roles: Vec<_> = result
        .appended_messages
        .iter()
        .map(|message| message.role.as_str())
        .collect();
    assert_eq!(roles, ["assistant", "tool", "assistant"]);
}

#[tokio::test]
async fn progress_reports_phase_changes_and_quiet_activity_at_most_every_five_seconds() {
    let reasoning = ProviderEvent::Event {
        kind: "run.message.reasoning-delta".into(),
        data: json!({"delta":"thinking"}),
    };
    let (events, result) = run(vec![vec![
        event(ProviderEvent::Started),
        event(ProviderEvent::Activity),
        event(reasoning),
        Step::At(100),
        event(ProviderEvent::Activity),
        Step::At(1_100),
        event(ProviderEvent::Activity),
        Step::At(6_100),
        event(ProviderEvent::Activity),
        Step::At(11_099),
        event(ProviderEvent::Activity),
        Step::At(11_100),
        event(ProviderEvent::Delta("answer".into())),
        event(ProviderEvent::Activity),
        Step::At(13_000),
        progress(0),
        event(ProviderEvent::Activity),
        completed("answer", &[]),
    ]])
    .await;
    assert_eq!(result.terminal, Terminal::Completed);
    assert_eq!(
        kinds(&events),
        [
            "run.started",
            "run.message.reasoning-delta",
            "run.progress",
            "run.progress",
            "run.message.delta",
            "run.progress",
            "run.tool.preparing",
            "run.progress",
            "run.message.completed",
            "run.completed",
        ]
    );
    assert_eq!(
        data_of(&events, "run.progress"),
        ["reasoning", "reasoning", "generating", "tool_args"]
            .iter()
            .map(|phase| json!({"phase":phase,"step":1}))
            .collect::<Vec<_>>()
            .iter()
            .collect::<Vec<_>>()
    );
    assert_eq!(result.appended_messages.len(), 1);
}
