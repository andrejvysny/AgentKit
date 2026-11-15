//! TU-24: the common coding shape on the provider wire. A request snapshot
//! (what a chat-completions endpoint receives), a recorded response that
//! translates to the canonical call, and every coding seed task of the
//! conformance set passing on the mock model under the common wire names.
use agentkit_core::*;
use agentkit_providers::conformance::{
    MockMode, MockModel, Task, Thresholds, load_dir, run_matrix,
};
use agentkit_providers::*;
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::path::Path;
use std::sync::{Arc, Mutex};

const CODING_TOOLSETS: [&str; 3] = ["files", "shell", "git"];

fn seed_tasks() -> Vec<Task> {
    load_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("conformance/tasks")).expect("seed tasks")
}

fn coding_tasks() -> Vec<Task> {
    seed_tasks()
        .into_iter()
        .filter(|task| CODING_TOOLSETS.contains(&task.toolset.as_str()))
        .collect()
}

// ---- seed tasks on the mock model -----------------------------------------

#[test]
fn coding_seed_tasks_name_tools_as_the_common_dialect_does() {
    let dialect = common_dialect();
    let wires = dialect.wire_names();
    let mut seen_wire = std::collections::BTreeSet::new();
    for task in coding_tasks() {
        for tool in &task.allowed_tools {
            // A canonical id the dialect renames is not a name the model sees.
            if let Some(form) = dialect.tools.iter().find(|form| form.canonical == tool.name) {
                assert!(
                    form.wires.iter().any(|wire| wire.name == tool.name),
                    "{}: {} is a canonical id, the model sees {:?}",
                    task.id,
                    tool.name,
                    dialect.wire_names_of(&tool.name)
                );
            }
            if wires.contains(&tool.name) {
                seen_wire.insert(tool.name.clone());
            }
            // The common shape names the path parameter file_path.
            if matches!(tool.name.as_str(), "read_file" | "write_file") {
                assert!(tool.required.contains(&"file_path".to_owned()), "{}", task.id);
                assert!(!tool.required.contains(&"path".to_owned()), "{}", task.id);
            }
        }
    }
    for expected in ["read_file", "write_file", "run_shell_command", "git_status", "git_log"] {
        assert!(seen_wire.contains(expected), "no seed task offers {expected}");
    }
}

#[tokio::test]
async fn every_coding_seed_task_passes_on_the_mock_model_with_common_names() {
    let tasks = coding_tasks();
    assert_eq!(tasks.len(), 9);
    let perfect = MockModel::new("mock-perfect", MockMode::Perfect);
    let matrix = run_matrix(&[&perfect], &tasks, 3, Thresholds::default()).await;
    assert_eq!(matrix.cells.len(), CODING_TOOLSETS.len());
    for cell in &matrix.cells {
        assert_eq!((cell.tasks, cell.runs, cell.successes), (3, 3, 9), "{}", cell.toolset);
        assert!(cell.qualified, "{}", cell.toolset);
        assert_eq!(cell.invalid_calls, 0, "{}", cell.toolset);
    }
}

// ---- request snapshot and recorded response ---------------------------------

struct Transport {
    identity: ConnectionIdentity,
    replies: Mutex<Vec<String>>,
    sent: Mutex<Vec<Value>>,
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

fn fixture() -> Value {
    serde_json::from_str(include_str!("../../core/tests/fixtures/dialect-common-v1.json"))
        .expect("common dialect fixture")
}

fn canonical_definitions() -> Vec<ToolDefinition> {
    fixture()["canonical"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| serde_json::from_value(value.clone()).unwrap())
        .collect()
}

/// One round trip: the request body sent and the calls the model's reply holds.
async fn round_trip(reply: String) -> (Value, Vec<ToolCall>) {
    let transport = Arc::new(Transport {
        identity: ConnectionIdentity {
            provider_id: "common".into(),
            protocol: "chat-completions".into(),
            connection_id: "c".into(),
            generation: 1,
        },
        replies: Mutex::new(vec![reply]),
        sent: Mutex::new(vec![]),
    });
    let provider = ChatCompletionsProvider::new(transport.clone()).unwrap();
    let request = ChatRequest {
        run_id: "run".into(),
        model: "m".into(),
        messages: vec![ChatMessage::text("user", "go")],
        tools: common_dialect().render(&canonical_definitions()),
        ..Default::default()
    };
    let events: Vec<_> = provider
        .stream_chat(request, CancellationToken::new())
        .collect()
        .await;
    let calls = events
        .into_iter()
        .filter_map(|event| match event.expect("provider event") {
            ProviderEvent::Completed { tool_calls, .. } => Some(tool_calls),
            _ => None,
        })
        .flatten()
        .collect();
    let sent = transport.sent.lock().unwrap()[0].clone();
    (sent, calls)
}

fn reply_calling(name: &str, arguments: &str) -> String {
    sse(&[
        json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1",
            "function":{"name":name,"arguments":arguments}}]}}]}),
        json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}),
    ]) + "data: [DONE]\n\n"
}

#[tokio::test]
async fn the_request_carries_the_common_tools_and_nothing_canonical() {
    let (sent, _) = round_trip(reply_calling("git_status", "{}")).await;
    let tools = sent["tools"].as_array().expect("tools");
    let shape: Vec<(String, Vec<String>)> = tools
        .iter()
        .map(|tool| {
            let function = &tool["function"];
            let mut keys: Vec<String> = function["parameters"]["properties"]
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect();
            keys.sort();
            (function["name"].as_str().unwrap().to_owned(), keys)
        })
        .collect();
    let owned = |name: &str, keys: &[&str]| {
        (
            name.to_owned(),
            keys.iter().map(|key| (*key).to_owned()).collect::<Vec<_>>(),
        )
    };
    assert_eq!(
        shape,
        [
            owned("edit", &["file_path", "new_string", "old_string", "replace_all"]),
            owned("git_status", &["path"]),
            owned("git_diff", &["path", "rev", "staged"]),
            owned("git_log", &["limit", "path", "rev"]),
            owned("git_show", &["path", "rev"]),
            owned("calendar_list", &["day"]),
        ]
    );
    // The body holds the rendered definitions exactly, and no canonical id.
    let rendered = common_dialect().render(&canonical_definitions());
    for (wire, definition) in tools.iter().zip(&rendered) {
        assert_eq!(wire["function"]["parameters"], definition.input_schema);
        assert_eq!(
            wire["function"]["description"],
            presented_description(definition, ExampleLane::ChatCompletions)
        );
    }
    let text = sent["tools"].to_string();
    assert!(!text.contains("git_inspect") && !text.contains("edit_file"), "{text}");
}

#[tokio::test]
async fn the_recorded_response_translates_to_the_canonical_call() {
    let fixture = fixture();
    let dialect = common_dialect();
    for pair in fixture["response"]["calls"].as_array().unwrap() {
        let wire = &pair["wire"];
        let arguments = wire["arguments"].to_string();
        let (_, calls) = round_trip(reply_calling(wire["name"].as_str().unwrap(), &arguments)).await;
        assert_eq!(calls.len(), 1, "{wire}");
        assert_eq!(calls[0].name, wire["name"], "{wire}");
        let args: Value = serde_json::from_str(&calls[0].arguments_json).unwrap();
        let translated = dialect.translate(&calls[0].name, args).unwrap();
        assert_eq!(translated.name, pair["canonical"]["name"], "{wire}");
        assert_eq!(translated.args, pair["canonical"]["arguments"], "{wire}");
    }
}
