//! A tool call that fails with identical arguments in three separate model
//! steps ends the run (`finishReason: "repeated_failure"`) once that step's
//! results are recorded.
use agentkit_core::*;
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

const STOPPED: &str =
    "The same tool call failed three times with identical arguments; the run was stopped.";

/// Answers request n with the calls of step n, then with a final answer.
struct Script {
    steps: Vec<Vec<ToolCall>>,
    requests: Mutex<usize>,
}
#[async_trait]
impl ChatProvider for Script {
    fn id(&self) -> &str {
        "test"
    }
    fn stream_chat(&self, _: ChatRequest, _: CancellationToken) -> ProviderStream {
        let mut requests = self.requests.lock().unwrap();
        *requests += 1;
        let event = match self.steps.get(*requests - 1) {
            Some(calls) => ProviderEvent::Completed {
                content: String::new(),
                tool_calls: calls.clone(),
                finish_reason: "tool_calls".into(),
            },
            None => ProviderEvent::Completed {
                content: "done".into(),
                tool_calls: vec![],
                finish_reason: "stop".into(),
            },
        };
        Box::pin(futures_util::stream::iter([Ok(event)]))
    }
}

struct Echo {
    definition: ToolDefinition,
}
#[async_trait]
impl Tool for Echo {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }
    async fn execute(&self, context: ToolContext, _: Value) -> Result<ToolResult, CoreError> {
        Ok(ToolResult {
            ok: true,
            data: Some(json!({"ok": true})),
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

/// `edit_file` requires `file_path`, so `{}` fails validation every time.
fn registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    for name in ["edit_file", "read_file"] {
        let definition = serde_json::from_value(json!({"name":name,"version":"1","effect":"read",
            "capability":"test","description":"d","inputSchema":
            {"type":"object","required":["file_path"],"properties":{"file_path":{"type":"string"}}}}))
        .unwrap();
        registry.register(Arc::new(Echo { definition })).unwrap();
    }
    registry
}

fn call(id: &str, name: &str, args: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments_json: args.into(),
    }
}

struct Outcome {
    events: Vec<Value>,
    requests: usize,
    messages: Vec<ChatMessage>,
}
impl Outcome {
    fn of(&self, kind: &str) -> Vec<&Value> {
        self.events
            .iter()
            .filter(|event| event["type"] == kind)
            .map(|event| &event["data"])
            .collect()
    }
    fn finish(&self) -> &Value {
        &self.of("run.completed")[0]["finishReason"]
    }
    fn stopped(&self) -> bool {
        self.of("run.warning")
            .iter()
            .any(|warning| warning["code"] == "repeated_failure")
    }
}

async fn run(steps: Vec<Vec<ToolCall>>, cap: usize) -> Outcome {
    let provider = Arc::new(Script {
        steps,
        requests: Mutex::new(0),
    });
    let handle = run_chat(
        RunOptions {
            request: ChatRequest {
                model: "m".into(),
                ..Default::default()
            },
            tool_call_records: true,
            max_tool_iterations: 8,
            max_tool_calls_per_iteration: cap,
            ..Default::default()
        },
        provider.clone(),
        Arc::new(registry()),
    );
    let events = handle
        .events
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .map(|event| serde_json::to_value(event).unwrap())
        .collect();
    let messages = handle.result.await.unwrap().appended_messages;
    let requests = *provider.requests.lock().unwrap();
    Outcome {
        events,
        requests,
        messages,
    }
}

#[tokio::test]
async fn three_identical_failures_across_steps_end_the_run() {
    let outcome = run(
        vec![
            vec![call("c1", "edit_file", "{}")],
            vec![call("c2", "edit_file", "{}")],
            vec![call("c3", "edit_file", "{}")],
            vec![call("c4", "read_file", r#"{"file_path":"a"}"#)],
        ],
        8,
    )
    .await;
    assert_eq!(outcome.requests, 3, "no fourth step is asked for");
    assert_eq!(outcome.of("run.tool.failed").len(), 3);
    let warning = outcome
        .of("run.warning")
        .into_iter()
        .find(|warning| warning["code"] == "repeated_failure")
        .expect("repeated_failure warning");
    assert_eq!(warning["message"], STOPPED);
    let completed = outcome.of("run.completed");
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0]["finishReason"], "repeated_failure");
    assert_eq!(completed[0]["iterations"], 3);
}

#[tokio::test]
async fn identical_failures_in_one_step_count_once_and_the_model_is_asked_again() {
    let outcome = run(
        vec![
            (1..=4)
                .map(|n| call(&format!("c{n}"), "edit_file", "{}"))
                .collect(),
        ],
        8,
    )
    .await;
    // Every call of the step is answered, in order, and the model reads them.
    let answered: Vec<&str> = outcome
        .messages
        .iter()
        .filter(|message| message.role == "tool")
        .map(|message| message.tool_call_id.as_deref().unwrap())
        .collect();
    assert_eq!(answered, ["c1", "c2", "c3", "c4"]);
    assert_eq!(outcome.of("run.tool.failed").len(), 4);
    assert_eq!(outcome.requests, 2);
    assert!(!outcome.stopped());
    assert_eq!(outcome.finish(), "stop");
}

#[tokio::test]
async fn identical_failures_end_the_run_in_the_third_separate_step() {
    let outcome = run(
        vec![
            vec![call("c1", "edit_file", "{}"), call("c2", "edit_file", "{}")],
            vec![call("c3", "edit_file", "{}")],
            vec![call("c4", "edit_file", "{}")],
            vec![call("c5", "read_file", r#"{"file_path":"a"}"#)],
        ],
        8,
    )
    .await;
    assert_eq!(
        outcome.requests, 3,
        "step 2 does not end the run, step 3 does"
    );
    assert_eq!(outcome.of("run.tool.failed").len(), 4);
    assert!(outcome.stopped());
    let completed = outcome.of("run.completed");
    assert_eq!(completed[0]["finishReason"], "repeated_failure");
    assert_eq!(completed[0]["iterations"], 3);
}

#[tokio::test]
async fn two_identical_failures_then_a_different_call_do_not_end_the_run() {
    let outcome = run(
        vec![
            vec![call("c1", "edit_file", "{}")],
            vec![call("c2", "edit_file", "{}")],
            vec![call("c3", "edit_file", r#"{"file_path":1}"#)],
        ],
        8,
    )
    .await;
    assert_eq!(outcome.requests, 4);
    assert!(!outcome.stopped());
    assert_eq!(outcome.finish(), "stop");
}

#[tokio::test]
async fn identical_calls_that_succeed_never_end_the_run() {
    let same = r#"{"file_path":"a"}"#;
    let outcome = run(
        vec![
            vec![call("c1", "edit_file", same), call("c2", "edit_file", same)],
            vec![call("c3", "edit_file", same), call("c4", "edit_file", same)],
        ],
        8,
    )
    .await;
    assert_eq!(outcome.requests, 3);
    assert!(!outcome.stopped());
    assert_eq!(outcome.finish(), "stop");
}

#[tokio::test]
async fn identical_calls_skipped_by_the_call_cap_do_not_end_the_run() {
    let same = r#"{"file_path":"a"}"#;
    let outcome = run(
        vec![
            (1..=4)
                .map(|n| call(&format!("c{n}"), "read_file", same))
                .collect(),
        ],
        1,
    )
    .await;
    let records = outcome.of("run.tool.record");
    assert_eq!(records[3]["failureCode"], "tool_call_cap");
    assert_eq!(records[3]["outcomeClass"], "repeat_identical");
    assert_eq!(outcome.requests, 2);
    assert!(!outcome.stopped());
    assert_eq!(outcome.finish(), "stop");
}
