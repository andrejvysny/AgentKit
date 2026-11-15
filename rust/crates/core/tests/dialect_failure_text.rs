//! T1.7: failure text names tools and calls through the session dialect.
//! A tool writes `{{tool:<id>}}` or `{{call:<id> key=value}}`; the model
//! reads the wire names it was offered, never a canonical id or a parameter
//! a split tool no longer has.
use agentkit_core::*;
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

const TEXT: &str = "staged only applies to {{call:git_inspect command=diff}}. Next: call {{call:git_inspect command=diff}} instead; see {{tool:git_inspect}}.";

#[test]
fn split_dialects_name_the_wire_tool_and_canonical_text_names_the_call() {
    for dialect in [common_dialect(), anthropic_dialect(), openai_dialect(), openai_chat_dialect()] {
        let shown = render_dialect_text(Some(&dialect), TEXT);
        assert!(shown.contains("applies to git_diff."), "{}: {shown}", dialect.id);
        assert!(shown.contains("call git_diff instead"), "{}: {shown}", dialect.id);
        assert!(
            shown.contains("see git_status, git_diff, git_log or git_show."),
            "{}: {shown}",
            dialect.id
        );
        assert!(!shown.contains("{{") && !shown.contains("command"), "{shown}");
    }
    // No dialect (a session frozen before dialects): canonical wording.
    let canonical = render_dialect_text(None, TEXT);
    assert!(
        canonical.contains("applies to git_inspect with command \"diff\"."),
        "{canonical}"
    );
    assert!(canonical.contains("see git_inspect."), "{canonical}");
}

#[test]
fn renamed_tools_follow_the_dialect_and_unknown_placeholders_stay() {
    let common = common_dialect();
    assert_eq!(render_dialect_text(Some(&common), "use {{tool:edit_file}}"), "use edit");
    assert_eq!(render_dialect_text(None, "use {{tool:edit_file}}"), "use edit_file");
    assert_eq!(render_dialect_text(Some(&common), "{{what:x}} {{tool:no_such"), "{{what:x}} {{tool:no_such");
    assert_eq!(render_dialect_text(Some(&common), "plain"), "plain");
    // A call no wire tool fixes keeps the parameter wording, under the shown name.
    assert_eq!(
        render_dialect_text(Some(&common), "{{call:git_inspect command=blame}}"),
        "git_status, git_diff, git_log or git_show with command \"blame\""
    );
}

struct Script {
    calls: Vec<ToolCall>,
    asked: Mutex<usize>,
}

#[async_trait]
impl ChatProvider for Script {
    fn id(&self) -> &str {
        "test"
    }
    fn stream_chat(&self, _: ChatRequest, _: CancellationToken) -> ProviderStream {
        let mut asked = self.asked.lock().unwrap();
        *asked += 1;
        let event = if *asked == 1 {
            ProviderEvent::Completed {
                content: String::new(),
                tool_calls: self.calls.clone(),
                finish_reason: "tool_calls".into(),
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

/// A tool whose body refuses with placeholder text, as the git tool does.
struct Refusing {
    definition: ToolDefinition,
}

#[async_trait]
impl Tool for Refusing {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }
    async fn execute(&self, context: ToolContext, _: Value) -> Result<ToolResult, CoreError> {
        Ok(ToolResult {
            ok: false,
            data: Some(json!({"hint": TEXT})),
            sources: vec![],
            warnings: vec![],
            truncated: false,
            limits: context.limits,
            model_data: None,
            summary: Some(format!("git_staged_needs_diff: {TEXT}")),
            status: None,
            model_content: None,
        })
    }
}

async fn failure_under(dialect: Option<Dialect>, name: &str, arguments: &str) -> Value {
    let canonical: Vec<ToolDefinition> = serde_json::from_str::<Value>(include_str!(
        "fixtures/dialect-common-v1.json"
    ))
    .unwrap()["canonical"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| serde_json::from_value(value.clone()).unwrap())
        .filter(|definition: &ToolDefinition| definition.name == "git_inspect")
        .collect();
    let mut registry = ToolRegistry::new();
    for definition in canonical {
        registry.register(Arc::new(Refusing { definition })).unwrap();
    }
    if let Some(dialect) = dialect {
        let rendered = dialect.render(&registry.definitions());
        registry.freeze_presented(rendered, vec![], dialect);
    }
    let provider = Arc::new(Script {
        calls: vec![ToolCall {
            id: "c1".into(),
            name: name.into(),
            arguments_json: arguments.into(),
        }],
        asked: Mutex::new(0),
    });
    let handle = run_chat(
        RunOptions {
            request: ChatRequest {
                model: "m".into(),
                ..Default::default()
            },
            ..Default::default()
        },
        provider,
        Arc::new(registry),
    );
    let events: Vec<Value> = handle
        .events
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .map(|event| serde_json::to_value(event).unwrap())
        .collect();
    events
        .into_iter()
        .find(|event| event["type"] == "run.tool.failed")
        .expect("the tool failed")["data"]
        .clone()
}

#[tokio::test]
async fn the_pipeline_renders_a_tool_failure_for_the_session_dialect() {
    let shown = failure_under(Some(common_dialect()), "git_diff", r#"{"staged":true}"#).await;
    let message = shown["errorMessage"].as_str().unwrap();
    assert!(message.contains("applies to git_diff."), "{message}");
    assert!(!message.contains("{{"), "{message}");
    let model = shown.to_string();
    assert!(!model.contains("{{"), "{model}");
    // A session without a dialect gets the canonical wording.
    let canonical = failure_under(None, "git_inspect", r#"{"command":"log","staged":true}"#).await;
    let message = canonical["errorMessage"].as_str().unwrap();
    assert!(message.contains("git_inspect with command \"diff\""), "{message}");
}
