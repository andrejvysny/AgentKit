//! Call pipeline (TU-08, TU-12): resolve, normalise, record, result order.
use agentkit_core::*;
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

struct Script {
    calls: Vec<ToolCall>,
    finish: &'static str,
    requests: Mutex<Vec<Vec<ChatMessage>>>,
}
#[async_trait]
impl ChatProvider for Script {
    fn id(&self) -> &str {
        "test"
    }
    fn stream_chat(&self, request: ChatRequest, _: CancellationToken) -> ProviderStream {
        let mut seen = self.requests.lock().unwrap();
        seen.push(request.messages);
        let event = if seen.len() == 1 {
            ProviderEvent::Completed {
                content: String::new(),
                tool_calls: self.calls.clone(),
                finish_reason: self.finish.into(),
            }
        } else {
            ProviderEvent::Completed {
                content: "done".into(),
                tool_calls: vec![],
                finish_reason: "stop".into(),
            }
        };
        Box::pin(futures_util::stream::iter([Ok(event)]))
    }
}

struct Echo {
    definition: ToolDefinition,
    received: Arc<Mutex<Vec<Value>>>,
}
#[async_trait]
impl Tool for Echo {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }
    async fn execute(&self, context: ToolContext, input: Value) -> Result<ToolResult, CoreError> {
        self.received.lock().unwrap().push(input);
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

fn call(id: &str, name: &str, args: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments_json: args.into(),
    }
}

fn registry(received: &Arc<Mutex<Vec<Value>>>) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    for (name, schema) in [
        (
            "edit_file",
            json!({"type":"object","required":["file_path"],"properties":{
                "file_path":{"type":"string"},"limit":{"type":"integer"},"timeout":{"type":"integer"}}}),
        ),
        ("read_file", json!({"type":"object"})),
        ("write_file", json!({"type":"object"})),
        ("search", json!({"type":"object"})),
    ] {
        let definition = serde_json::from_value(json!({"name":name,"version":"1","effect":"read",
            "capability":"test","description":"d","inputSchema":schema}))
        .unwrap();
        registry
            .register(Arc::new(Echo {
                definition,
                received: received.clone(),
            }))
            .unwrap();
    }
    registry
}

struct Outcome {
    events: Vec<Value>,
    requests: Vec<Vec<ChatMessage>>,
}
async fn run(registry: ToolRegistry, calls: Vec<ToolCall>) -> Outcome {
    run_finishing(registry, calls, "tool_calls").await
}
async fn run_finishing(
    registry: ToolRegistry,
    calls: Vec<ToolCall>,
    finish: &'static str,
) -> Outcome {
    let provider = Arc::new(Script {
        calls,
        finish,
        requests: Mutex::new(vec![]),
    });
    let handle = run_chat(
        RunOptions {
            request: ChatRequest {
                model: "m".into(),
                ..Default::default()
            },
            tool_call_records: true,
            max_tool_calls_per_iteration: 8,
            ..Default::default()
        },
        provider.clone(),
        Arc::new(registry),
    );
    let events = handle
        .events
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .map(|event| serde_json::to_value(event).unwrap())
        .collect();
    let requests = provider.requests.lock().unwrap().clone();
    Outcome { events, requests }
}
fn of_type<'a>(outcome: &'a Outcome, kind: &str) -> Vec<&'a Value> {
    outcome
        .events
        .iter()
        .filter(|event| event["type"] == kind)
        .map(|event| &event["data"])
        .collect()
}

#[tokio::test]
async fn alias_and_text_values_reach_the_tool_canonical_and_are_recorded() {
    let received = Arc::new(Mutex::new(vec![]));
    let outcome = run(
        registry(&received),
        vec![call(
            "c1",
            "edit_file",
            r#"{"path":"a.txt","limit":"5","timeout_sec":2}"#,
        )],
    )
    .await;
    assert_eq!(
        received.lock().unwrap()[0],
        json!({"file_path":"a.txt","limit":5,"timeout":2000})
    );
    let record = of_type(&outcome, "run.tool.record")[0];
    assert_eq!(record["outcomeClass"], "ok");
    assert_eq!(
        record["aliasesUsed"],
        json!(["path->file_path", "timeout_sec->timeout*1000"])
    );
    assert_eq!(record["valuesConverted"], 1);
}

#[tokio::test]
async fn alias_never_fills_a_missing_required_value() {
    let received = Arc::new(Mutex::new(vec![]));
    let outcome = run(
        registry(&received),
        vec![call("c1", "edit_file", r#"{"limit":"5"}"#)],
    )
    .await;
    assert!(received.lock().unwrap().is_empty());
    let record = of_type(&outcome, "run.tool.record")[0];
    assert_eq!(record["outcomeClass"], "invalid_structure");
    assert_eq!(record["valuesConverted"], 1);
}

#[tokio::test]
async fn unknown_name_lists_three_closest_names_and_points_to_tool_search() {
    let received = Arc::new(Mutex::new(vec![]));
    let outcome = run(registry(&received), vec![call("c1", "raed_file", "{}")]).await;
    let failed = of_type(&outcome, "run.tool.failed")[0];
    let message = failed["errorMessage"].as_str().unwrap();
    assert_eq!(failed["errorCode"], "tool_missing");
    assert!(
        message.contains("read_file, "),
        "closest name first: {message}"
    );
    for name in ["read_file", "edit_file", "write_file"] {
        assert!(message.contains(name), "{message}");
    }
    assert!(!message.contains(", search"), "only three names: {message}");
    assert!(message.contains("tool_search"), "{message}");
    assert_eq!(
        of_type(&outcome, "run.tool.record")[0]["outcomeClass"],
        "unknown_tool"
    );
}

#[tokio::test]
async fn withheld_tool_says_so_and_names_alternatives() {
    let received = Arc::new(Mutex::new(vec![]));
    let mut registry = registry(&received);
    registry.withhold("delete_file", "this turn is read-only");
    let outcome = run(registry, vec![call("c1", "delete_file", "{}")]).await;
    let failed = of_type(&outcome, "run.tool.failed")[0];
    let message = failed["errorMessage"].as_str().unwrap();
    assert_eq!(failed["errorCode"], "tool_not_allowed");
    assert!(message.contains("not available this turn"), "{message}");
    assert!(message.contains("this turn is read-only"), "{message}");
    assert!(message.contains("read_file") || message.contains("write_file"));
    assert_eq!(
        of_type(&outcome, "run.tool.record")[0]["outcomeClass"],
        "guard_refused"
    );
}

#[tokio::test]
async fn unnamed_call_gets_a_result_that_says_the_name_was_missing() {
    let received = Arc::new(Mutex::new(vec![]));
    let outcome = run(registry(&received), vec![call("c1", UNNAMED_TOOL, "{}")]).await;
    let failed = of_type(&outcome, "run.tool.failed")[0];
    assert_eq!(failed["errorCode"], "tool_name_missing");
    assert!(failed["errorMessage"].as_str().unwrap().contains("no name"));
    assert_eq!(
        of_type(&outcome, "run.tool.record")[0]["outcomeClass"],
        "invalid_structure"
    );
}

#[tokio::test]
async fn results_of_one_turn_are_contiguous_and_in_call_order() {
    let received = Arc::new(Mutex::new(vec![]));
    let outcome = run(
        registry(&received),
        vec![
            call("c1", "read_file", "{}"),
            call("c2", "no_such_tool", "{}"),
            call("c3", "search", "{}"),
        ],
    )
    .await;
    let next = &outcome.requests[1];
    let at = next
        .iter()
        .position(|message| message.role == "assistant" && !message.tool_calls.is_empty())
        .expect("assistant tool call message");
    let ids: Vec<&str> = next[at + 1..]
        .iter()
        .map(|message| {
            assert_eq!(message.role, "tool");
            message.tool_call_id.as_deref().unwrap()
        })
        .collect();
    assert_eq!(ids, ["c1", "c2", "c3"]);
    assert!(
        next[at + 2]
            .content
            .as_str()
            .unwrap()
            .contains("tool_search"),
        "the failing call carries its own failure in place"
    );
}

const CUT_OFF_HINT: &str = "Your output reached its limit before this call was complete.";
fn tool_result(outcome: &Outcome) -> String {
    let message = outcome.requests[1]
        .iter()
        .find(|message| message.role == "tool")
        .expect("tool result message");
    message.content.as_str().unwrap().to_owned()
}

#[tokio::test]
async fn cut_off_call_tells_the_model_to_send_smaller_parts() {
    let received = Arc::new(Mutex::new(vec![]));
    let cut = call("c1", "write_file", r#"{"file_path":"a.txt","content":"par"#);
    let outcome = run_finishing(registry(&received), vec![cut], "length").await;
    assert_eq!(
        of_type(&outcome, "run.tool.failed")[0]["errorCode"],
        "bad_args"
    );
    let result = tool_result(&outcome);
    assert!(result.contains(CUT_OFF_HINT), "{result}");
    assert!(result.contains("several shorter calls"), "{result}");
}

#[tokio::test]
async fn bad_arguments_without_a_cut_off_get_no_cut_off_hint() {
    let received = Arc::new(Mutex::new(vec![]));
    let cut = call("c1", "write_file", r#"{"file_path":"a.txt","content":"par"#);
    let outcome = run_finishing(registry(&received), vec![cut], "tool_calls").await;
    assert_eq!(
        of_type(&outcome, "run.tool.failed")[0]["errorCode"],
        "bad_args"
    );
    let result = tool_result(&outcome);
    assert!(!result.contains(CUT_OFF_HINT), "{result}");
}
