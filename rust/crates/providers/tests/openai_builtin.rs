//! TU-26 on the Responses lane: the patch and shell tools as typed tools, the
//! vendor's call items parsed into calls, results returned as the vendor's
//! output items, and strict flags on the schemas that are strict. The vendor
//! shapes are recorded in `core/tests/fixtures/dialect-openai-v1.json`.
use agentkit_core::*;
use agentkit_providers::*;
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

fn fixture() -> Value {
    serde_json::from_str(include_str!(
        "../../core/tests/fixtures/dialect-openai-v1.json"
    ))
    .expect("fixture")
}

struct Transport {
    identity: ConnectionIdentity,
    replies: Mutex<Vec<String>>,
    sent: Mutex<Vec<Value>>,
}
impl Transport {
    fn new(replies: Vec<String>) -> Arc<Self> {
        Arc::new(Self {
            identity: ConnectionIdentity {
                provider_id: "openai-builtin".into(),
                protocol: "responses".into(),
                connection_id: "c".into(),
                generation: 1,
            },
            replies: Mutex::new(replies),
            sent: Mutex::new(vec![]),
        })
    }
}
#[async_trait]
impl TrustedTransport for Transport {
    fn identity(&self) -> ConnectionIdentity {
        self.identity.clone()
    }
    fn auth_kind(&self) -> AuthKind {
        AuthKind::ApiKey
    }
    fn check_current(&self) -> Result<(), CoreError> {
        Ok(())
    }
    async fn request(
        &self,
        _: &ConnectionIdentity,
        _: &str,
        body: Option<Value>,
        _: CancellationToken,
    ) -> Result<reqwest::Response, CoreError> {
        self.sent.lock().unwrap().push(body.unwrap_or(Value::Null));
        let body = self.replies.lock().unwrap().remove(0);
        Ok(http::Response::builder()
            .status(200)
            .header("content-type", "text/event-stream")
            .body(body)
            .unwrap()
            .into())
    }
}

fn sse(values: &[Value]) -> String {
    values
        .iter()
        .map(|value| format!("data: {value}\n\n"))
        .collect()
}
fn completed(output: Vec<Value>) -> String {
    sse(&[json!({"type":"response.completed","response":{"status":"completed","output":output}})])
}
fn answer() -> String {
    completed(vec![
        json!({"id":"m1","type":"message","role":"assistant","status":"completed",
        "content":[{"type":"output_text","text":"done"}]}),
    ])
}

/// A canonical coding tool that succeeds the way the real one reports.
struct Coding {
    definition: ToolDefinition,
    text: &'static str,
    summary: &'static str,
}
#[async_trait]
impl Tool for Coding {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }
    async fn execute(&self, context: ToolContext, _: Value) -> Result<ToolResult, CoreError> {
        Ok(ToolResult {
            ok: true,
            data: None,
            sources: vec![],
            warnings: vec![],
            truncated: false,
            limits: context.limits,
            model_data: Some(Value::String(self.text.into())),
            summary: Some(self.summary.into()),
            status: None,
            model_content: None,
        })
    }
}

fn definitions() -> Vec<ToolDefinition> {
    fixture()["canonical"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| serde_json::from_value(value.clone()).unwrap())
        .collect()
}

fn registry() -> Arc<ToolRegistry> {
    let mut registry = ToolRegistry::new();
    for definition in definitions().into_iter().take(3) {
        let (text, summary) = match definition.name.as_str() {
            "run_command" => ("total 0\n[exit code 0]", "exit code 0"),
            _ => ("changed", "changed a.rs"),
        };
        registry
            .register(Arc::new(Coding {
                definition,
                text,
                summary,
            }))
            .unwrap();
    }
    let dialect = openai_dialect();
    let rendered = dialect.render(&registry.definitions());
    assert!(registry.freeze_presented(rendered, vec![], dialect));
    Arc::new(registry)
}

async fn run(transport: &Arc<Transport>) -> Vec<Value> {
    let provider = ResponsesProvider::new(transport.clone()).unwrap();
    let handle = run_chat(
        RunOptions {
            request: ChatRequest {
                model: "m".into(),
                messages: vec![ChatMessage::text("user", "go")],
                ..Default::default()
            },
            tool_call_records: true,
            ..Default::default()
        },
        Arc::new(provider),
        registry(),
    );
    handle
        .events
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .map(|event| serde_json::to_value(event).unwrap())
        .collect()
}

#[tokio::test]
async fn the_request_offers_the_typed_tools_and_a_whole_loop_answers_in_vendor_items() {
    let items = fixture()["response"]["items"].clone();
    let transport = Transport::new(vec![
        completed(vec![
            items[0].clone(),
            // The shell call as the vendor shows it: no item id, still in progress.
            items[1].clone(),
        ]),
        answer(),
    ]);
    let events = run(&transport).await;
    let sent = transport.sent.lock().unwrap().clone();
    assert_eq!(sent.len(), 2, "{events:?}");

    // Request 1: the typed tools, outside the application namespace.
    let tools = sent[0]["tools"].as_array().unwrap();
    assert_eq!(
        tools[..2],
        fixture()["request"]["responsesTools"].as_array().unwrap()[..]
    );
    assert_eq!(tools.len(), 2, "{tools:?}");

    // The calls reached the canonical tools.
    let started: Vec<&str> = events
        .iter()
        .filter(|event| event["type"] == "run.tool.running")
        .map(|event| event["data"]["toolName"].as_str().unwrap())
        .collect();
    assert_eq!(started, ["edit_file", "run_command"], "{events:?}");

    // Request 2: the calls and their outputs in the vendor's item shapes.
    let input = sent[1]["input"].as_array().unwrap();
    let kind = |name: &str| input.iter().find(|item| item["type"] == name);
    let call = kind("apply_patch_call").expect("patch call echoed");
    assert_eq!(call["call_id"], items[0]["call_id"]);
    assert_eq!(call["operation"]["type"], "update_file");
    assert_eq!(call["operation"]["path"], "lib/fib.py");
    let output = kind("apply_patch_call_output").expect("patch output");
    assert_eq!(output["call_id"], items[0]["call_id"]);
    assert_eq!(output["status"], "completed");
    assert_eq!(output["output"], "changed a.rs");
    let shell = kind("shell_call").expect("shell call echoed");
    assert_eq!(shell["call_id"], items[1]["call_id"]);
    assert_eq!(shell["action"]["commands"], json!(["ls -l"]));
    let output = kind("shell_call_output").expect("shell output");
    assert_eq!(output["call_id"], items[1]["call_id"]);
    assert_eq!(
        output["output"],
        json!([{"stdout":"total 0\n[exit code 0]","stderr":"","outcome":{"type":"exit","exit_code":0}}])
    );
    assert!(kind("function_call_output").is_none(), "{input:?}");
}

#[tokio::test]
async fn a_refused_patch_is_answered_as_a_failed_output() {
    let delete = json!({"id":"apc_1","type":"apply_patch_call","status":"completed","call_id":"call_d",
        "operation":{"type":"delete_file","path":"a.rs"}});
    let transport = Transport::new(vec![completed(vec![delete]), answer()]);
    let events = run(&transport).await;
    let sent = transport.sent.lock().unwrap().clone();
    let output = sent[1]["input"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["type"] == "apply_patch_call_output")
        .unwrap_or_else(|| panic!("{events:?}"));
    assert_eq!(output["call_id"], "call_d");
    assert_eq!(output["status"], "failed");
    assert!(
        output["output"].as_str().unwrap().contains("delete_file"),
        "{output}"
    );
}

#[tokio::test]
async fn a_built_in_call_the_request_did_not_offer_is_refused_by_the_parser() {
    // A plain registry offers no patch tool; the vendor item has no tool to map to.
    let mut registry = ToolRegistry::new();
    registry
        .register(Arc::new(Coding {
            definition: definitions().remove(3),
            text: "x",
            summary: "x",
        }))
        .unwrap();
    let items = fixture()["response"]["items"].clone();
    let transport = Transport::new(vec![completed(vec![items[0].clone()]), answer()]);
    let provider = ResponsesProvider::new(transport.clone()).unwrap();
    let handle = run_chat(
        RunOptions {
            request: ChatRequest {
                model: "m".into(),
                messages: vec![ChatMessage::text("user", "go")],
                ..Default::default()
            },
            ..Default::default()
        },
        Arc::new(provider),
        Arc::new(registry),
    );
    let events: Vec<Value> = handle
        .events
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .map(|event| serde_json::to_value(event).unwrap())
        .collect();
    assert_eq!(transport.sent.lock().unwrap().len(), 1, "{events:?}");
    assert!(
        events.iter().any(|event| event["type"] == "run.failed"
            || event["type"] == "run.error"
            || event.to_string().contains("unknown_tool")),
        "{events:?}"
    );
}

/// A continued run (T3.3): the history of request 2 shows the emitted patch
/// call exactly as the provider returned it, so the continuation digest taken
/// after request 1 still matches. A patch with context lines does not survive
/// a canonical round trip (old/new text, then a re-rendered diff).
#[tokio::test]
async fn a_continued_run_shows_the_emitted_patch_call_unchanged() {
    let diff = "@@ def fib\n def fib(n):\n-    return n\n+    return n + 0\n     # end\n";
    let patch = json!({"id":"apc_c","type":"apply_patch_call","status":"completed","call_id":"call_ctx",
        "operation":{"type":"update_file","path":"lib/fib.py","diff":diff}});
    let transport = Transport::new(vec![completed(vec![patch]), answer()]);
    let provider = ResponsesProvider::new(transport.clone()).unwrap();
    let handle = run_chat(
        RunOptions {
            request: ChatRequest {
                model: "m".into(),
                messages: vec![ChatMessage::text("user", "go")],
                continuation_scope: Some(ContinuationScope {
                    chat_id: "chat".into(),
                    branch_id: "branch".into(),
                }),
                ..Default::default()
            },
            ..Default::default()
        },
        Arc::new(provider),
        registry(),
    );
    let events: Vec<Value> = handle
        .events
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .map(|event| serde_json::to_value(event).unwrap())
        .collect();
    assert!(
        events.iter().all(|event| event["type"] != "run.failed"
            && !event.to_string().contains("continuation_history_mismatch")),
        "{events:?}"
    );
    let sent = transport.sent.lock().unwrap().clone();
    assert_eq!(sent.len(), 2, "{events:?}");
    let input = sent[1]["input"].as_array().unwrap();
    let call = input
        .iter()
        .find(|item| item["type"] == "apply_patch_call")
        .expect("patch call echoed");
    assert_eq!(call["operation"]["diff"], diff);
    assert!(
        input
            .iter()
            .any(|item| item["type"] == "apply_patch_call_output")
    );
}

fn identity() -> ConnectionIdentity {
    ConnectionIdentity {
        provider_id: "p".into(),
        protocol: "responses".into(),
        connection_id: "c".into(),
        generation: 1,
    }
}

fn strict_request(profile: Option<&str>) -> Value {
    let rendered = openai_dialect().render(&definitions());
    let request = ChatRequest {
        run_id: "r".into(),
        model: "m".into(),
        messages: vec![ChatMessage::text("user", "hi")],
        tools: rendered,
        profile: profile.map(|id| Arc::new(builtin_profile(id).expect("profile"))),
        ..Default::default()
    };
    build_responses_request(&request, &identity(), false).expect("body")
}

#[test]
fn strict_is_set_on_strict_schemas_under_an_openai_profile_only() {
    let body = strict_request(Some("openai"));
    let functions = body["tools"][2]["tools"].as_array().unwrap();
    assert_eq!(functions[0]["name"], "calendar_list");
    assert_eq!(functions[0]["strict"], true);
    assert_eq!(
        functions[0]["parameters"]["properties"]["limit"]["type"],
        json!(["integer", "null"])
    );
    // No profile, or another family's: the lane stays non-strict as before.
    for profile in [None, Some("generic")] {
        let body = strict_request(profile);
        assert_eq!(body["tools"][2]["tools"][0]["strict"], false, "{profile:?}");
    }
}

#[test]
fn a_vendor_marker_the_responses_lane_does_not_know_is_refused() {
    let mut rendered = openai_dialect().render(&definitions());
    rendered[0].name = "bash".into();
    let request = ChatRequest {
        run_id: "r".into(),
        model: "m".into(),
        messages: vec![ChatMessage::text("user", "hi")],
        tools: rendered,
        ..Default::default()
    };
    let error = build_responses_request(&request, &identity(), false).unwrap_err();
    assert_eq!(error.code, "unsupported_tool");
}
