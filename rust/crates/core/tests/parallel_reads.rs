use agentkit_core::*;
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

type Log = Arc<Mutex<Vec<(String, &'static str, Instant)>>>;

struct Script(Mutex<Vec<Vec<ProviderEvent>>>);
#[async_trait]
impl ChatProvider for Script {
    fn id(&self) -> &str {
        "script"
    }
    fn stream_chat(&self, _: ChatRequest, _: CancellationToken) -> ProviderStream {
        let events = self.0.lock().unwrap().remove(0);
        Box::pin(futures_util::stream::iter(events.into_iter().map(Ok)))
    }
}

/// Sleeps `ms` on a tokio timer; `ask` makes it hand the run a human request.
struct Probe {
    definition: ToolDefinition,
    ms: u64,
    ask: bool,
    log: Log,
}
#[async_trait]
impl Tool for Probe {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }
    async fn execute(&self, context: ToolContext, _: Value) -> Result<ToolResult, CoreError> {
        let name = self.definition.name.clone();
        self.log
            .lock()
            .unwrap()
            .push((name.clone(), "start", Instant::now()));
        tokio::time::sleep(Duration::from_millis(self.ms)).await;
        self.log.lock().unwrap().push((name, "end", Instant::now()));
        if self.ask
            && let Some(slot) = &context.suspension
        {
            slot.set(HumanRequestSpec {
                request_id: "hreq:t:ask".into(),
                content: json!({"kind":"approval","toolName":"ask","prompt":"Allow?"}),
                content_hash: "h".into(),
                expires_at: None,
            });
        }
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

struct Sink;
#[async_trait]
impl SuspendSink for Sink {
    async fn park(&self, _: &HumanRequestSpec, _: &str) -> Result<(), CoreError> {
        Ok(())
    }
}

fn probe(name: &str, effect: &str, capability: &str, ms: u64, ask: bool, log: &Log) -> Arc<Probe> {
    Arc::new(Probe {
        definition: serde_json::from_value(json!({"name":name,"version":"1","effect":effect,
            "capability":capability,"description":"Probe.","inputSchema":{"type":"object"}}))
        .unwrap(),
        ms,
        ask,
        log: log.clone(),
    })
}

fn call(name: &str) -> ToolCall {
    ToolCall {
        id: format!("id_{name}"),
        name: name.into(),
        arguments_json: "{}".into(),
    }
}

struct Outcome {
    events: Vec<Value>,
    result: RunResult,
    elapsed: Duration,
}

async fn run(
    tools: Vec<Arc<Probe>>,
    calls: &[ToolCall],
    parallel: bool,
    cancel_after_ms: Option<u64>,
) -> Outcome {
    let mut registry = ToolRegistry::new();
    for tool in tools {
        registry.register(tool).unwrap();
    }
    let cancellation = CancellationToken::new();
    let mut options = RunOptions {
        cancellation: cancellation.clone(),
        parallel_read_tools: parallel,
        suspender: Some(Arc::new(Sink)),
        ..Default::default()
    };
    options.request.model = "m".into();
    options.request.messages = vec![ChatMessage::text("user", "hi")];
    let mut first = vec![
        ProviderEvent::Started,
        ProviderEvent::Event {
            kind: "run.message.completed".into(),
            data: json!({"content":"","toolCallCount":calls.len(),"toolCalls":calls}),
        },
    ];
    first.extend(calls.iter().cloned().map(ProviderEvent::ToolRequested));
    let second = vec![
        ProviderEvent::Delta("done".into()),
        ProviderEvent::Event {
            kind: "run.message.completed".into(),
            data: json!({"content":"done","toolCallCount":0,"toolCalls":[]}),
        },
    ];
    if let Some(ms) = cancel_after_ms {
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(ms)).await;
            cancellation.cancel();
        });
    }
    let started = Instant::now();
    let handle = run_chat(
        options,
        Arc::new(Script(Mutex::new(vec![first, second]))),
        Arc::new(registry),
    );
    let events = handle
        .events
        .map(|event| serde_json::to_value(event).unwrap())
        .collect::<Vec<_>>()
        .await;
    let result = handle.result.await.unwrap();
    Outcome {
        events,
        result,
        elapsed: started.elapsed(),
    }
}

fn tool_events(events: &[Value]) -> Vec<(String, String)> {
    events
        .iter()
        .filter(|event| {
            event["type"]
                .as_str()
                .is_some_and(|kind| kind.starts_with("run.tool.") && kind != "run.tool.requested")
        })
        .map(|event| {
            (
                event["type"].as_str().unwrap().to_owned(),
                event["data"]["toolCallId"]
                    .as_str()
                    .unwrap_or("")
                    .to_owned(),
            )
        })
        .collect()
}

fn reads(log: &Log) -> [Arc<Probe>; 2] {
    [
        probe("read_a", "read", "fs.read", 200, false, log),
        probe("read_b", "read", "fs.read", 200, false, log),
    ]
}

fn at(log: &Log, name: &str, phase: &str) -> Instant {
    log.lock()
        .unwrap()
        .iter()
        .find(|(entry, kind, _)| entry == name && *kind == phase)
        .unwrap()
        .2
}

#[tokio::test]
async fn two_slow_reads_overlap_and_report_in_call_order() {
    let log = Log::default();
    let outcome = run(
        reads(&log).to_vec(),
        &[call("read_a"), call("read_b")],
        true,
        None,
    )
    .await;
    assert_eq!(outcome.result.terminal, Terminal::Completed);
    assert!(
        outcome.elapsed < Duration::from_millis(350),
        "{:?}",
        outcome.elapsed
    );
    assert_eq!(
        tool_events(&outcome.events),
        [
            ("run.tool.running", "id_read_a"),
            ("run.tool.succeeded", "id_read_a"),
            ("run.tool.running", "id_read_b"),
            ("run.tool.succeeded", "id_read_b"),
        ]
        .map(|(kind, id)| (kind.to_owned(), id.to_owned()))
    );
    let ids: Vec<_> = outcome
        .result
        .appended_messages
        .iter()
        .filter(|message| message.role == "tool")
        .map(|message| message.tool_call_id.clone().unwrap())
        .collect();
    assert_eq!(ids, ["id_read_a", "id_read_b"]);
    assert!(at(&log, "read_b", "start") < at(&log, "read_a", "end"));
}

#[tokio::test]
async fn a_write_waits_for_the_read_batch_before_it_starts() {
    let log = Log::default();
    let mut tools = reads(&log).to_vec();
    tools.push(probe("write_c", "write", "fs.write", 10, false, &log));
    let outcome = run(
        tools,
        &[call("read_a"), call("read_b"), call("write_c")],
        true,
        None,
    )
    .await;
    assert_eq!(outcome.result.terminal, Terminal::Completed);
    let write = at(&log, "write_c", "start");
    assert!(write >= at(&log, "read_a", "end"));
    assert!(write >= at(&log, "read_b", "end"));
    assert!(at(&log, "read_b", "start") < at(&log, "read_a", "end"));
}

#[tokio::test]
async fn a_read_that_asks_a_human_parks_the_run_as_before() {
    let log = Log::default();
    let tools = vec![
        probe("read_a", "read", "fs.read", 50, false, &log),
        probe("read_ask", "read", "fs.read", 50, true, &log),
        probe("read_b", "read", "fs.read", 50, false, &log),
    ];
    let outcome = run(
        tools,
        &[call("read_a"), call("read_ask"), call("read_b")],
        true,
        None,
    )
    .await;
    assert!(outcome.result.suspended);
    let events = tool_events(&outcome.events);
    assert!(events.contains(&("run.tool.succeeded".into(), "id_read_a".into())));
    assert!(!events.iter().any(|(_, id)| id == "id_read_b"));
    assert!(outcome.events.iter().any(
        |event| event["type"] == "run.waiting" && event["data"]["toolCallId"] == "id_read_ask"
    ));
}

#[tokio::test]
async fn cancelling_mid_batch_drops_the_running_reads() {
    let log = Log::default();
    let tools = vec![
        probe("read_a", "read", "fs.read", 5_000, false, &log),
        probe("read_b", "read", "fs.read", 5_000, false, &log),
    ];
    let outcome = run(tools, &[call("read_a"), call("read_b")], true, Some(100)).await;
    assert_eq!(outcome.result.terminal, Terminal::Cancelled);
    assert!(
        outcome.elapsed < Duration::from_secs(2),
        "{:?}",
        outcome.elapsed
    );
    assert!(
        !tool_events(&outcome.events)
            .iter()
            .any(|(kind, _)| kind == "run.tool.succeeded")
    );
    assert_eq!(
        tool_events(&outcome.events)
            .iter()
            .filter(|(kind, _)| kind == "run.tool.failed")
            .count(),
        2
    );
}

#[tokio::test]
async fn with_the_option_off_reads_run_one_after_the_other() {
    let log = Log::default();
    let outcome = run(
        reads(&log).to_vec(),
        &[call("read_a"), call("read_b")],
        false,
        None,
    )
    .await;
    assert_eq!(outcome.result.terminal, Terminal::Completed);
    assert!(
        outcome.elapsed >= Duration::from_millis(400),
        "{:?}",
        outcome.elapsed
    );
    assert!(at(&log, "read_b", "start") >= at(&log, "read_a", "end"));
}

#[tokio::test]
async fn at_most_four_reads_run_at_once() {
    let log = Log::default();
    let names = ["r1", "r2", "r3", "r4", "r5"];
    let tools = names
        .iter()
        .map(|name| probe(name, "read", "fs.read", 150, false, &log))
        .collect();
    let calls: Vec<_> = names.iter().map(|name| call(name)).collect();
    let outcome = run(tools, &calls, true, None).await;
    assert_eq!(outcome.result.terminal, Terminal::Completed);
    assert!(
        outcome.elapsed >= Duration::from_millis(300),
        "{:?}",
        outcome.elapsed
    );
    assert!(at(&log, "r5", "start") >= at(&log, "r1", "end"));
}
