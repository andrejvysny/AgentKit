//! The OpenAI-style lane under a frozen model profile (T2.2): request
//! snapshots (TU-16), reasoning sent back (TU-17), message rules (TU-18),
//! salvage (TU-21) and the history sanitiser.

use agentkit_core::*;
use agentkit_providers::*;
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

struct Transport {
    identity: ConnectionIdentity,
    replies: Mutex<Vec<String>>,
    sent: Mutex<Vec<Value>>,
}
impl Transport {
    fn new(replies: Vec<String>) -> Arc<Self> {
        Arc::new(Self {
            identity: ConnectionIdentity {
                provider_id: "test".into(),
                protocol: "chat-completions".into(),
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

fn reply(content: &str) -> String {
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"delta":{"content":content},"finish_reason":"stop"}]})
    )
}

fn tool(name: &str) -> ToolDefinition {
    serde_json::from_value(
        json!({"name":name,"version":"1","effect":"read","capability":"test",
        "description":"d","inputSchema":{"type":"object"}}),
    )
    .unwrap()
}

fn qwen() -> Arc<AiModelToolProfile> {
    Arc::new(builtin_profile(QWEN_PROFILE_ID).unwrap())
}

fn request(profile: Option<Arc<AiModelToolProfile>>, messages: Vec<ChatMessage>) -> ChatRequest {
    ChatRequest {
        run_id: "run".into(),
        model: "qwen3.8-27b".into(),
        messages,
        tools: vec![tool("read_file")],
        profile,
        ..Default::default()
    }
}

struct Sent {
    body: Value,
    events: Vec<ProviderEvent>,
}
async fn send(input: ChatRequest, content: &str) -> Sent {
    let transport = Transport::new(vec![reply(content)]);
    let provider = ChatCompletionsProvider::new(transport.clone()).unwrap();
    let events: Vec<_> = provider
        .stream_chat(input, CancellationToken::new())
        .collect()
        .await;
    let events = events.into_iter().map(|event| event.unwrap()).collect();
    let body = transport.sent.lock().unwrap()[0].clone();
    Sent { body, events }
}
fn warnings(sent: &Sent) -> Vec<String> {
    sent.events
        .iter()
        .filter_map(|event| match event {
            ProviderEvent::Warning { code, .. } => Some(code.clone()),
            _ => None,
        })
        .collect()
}

fn layout() -> Vec<ChatMessage> {
    let mut messages =
        PromptLayout::of(Some("policy"), Some("rules"), Some("memory")).to_messages();
    messages.push(ChatMessage::text("user", "hi"));
    messages
}

#[tokio::test]
async fn qwen_request_snapshot() {
    let mut input = request(Some(qwen()), layout());
    input.reasoning_effort = Some(ReasoningEffort::High);
    input.temperature = Some(0.2);
    let sent = send(input, "ok").await;
    assert_eq!(
        sent.body,
        json!({
            "model": "qwen3.8-27b",
            // One system message, first: the layout blocks merged in order.
            "messages": [
                {"role": "system", "content": "policy\n\nrules\n\nmemory"},
                {"role": "user", "content": "hi"},
            ],
            "stream": true,
            "stream_options": {"include_usage": true},
            "temperature": 1.0,
            "top_p": 0.95,
            "top_k": 20,
            // The effort word is a template option, never OpenAI's field.
            "chat_template_kwargs": {"reasoning_effort": "xhigh"},
            "tools": [{"type": "function", "function": {
                "name": "read_file", "description": "d", "parameters": {"type": "object"}}}],
            "tool_choice": "auto",
        })
    );
    assert!(warnings(&sent).is_empty(), "{:?}", warnings(&sent));
}

#[tokio::test]
async fn qwen_with_thinking_off_snapshot_uses_the_other_sampling_set() {
    let mut input = request(Some(qwen()), layout());
    input.reasoning_off = true;
    let sent = send(input, "ok").await;
    for (key, value) in [
        ("temperature", json!(0.7)),
        ("top_p", json!(0.8)),
        ("top_k", json!(20)),
        ("presence_penalty", json!(1.5)),
        ("chat_template_kwargs", json!({"enable_thinking": false})),
    ] {
        assert_eq!(sent.body[key], value, "{key}");
    }
    assert!(sent.body.get("reasoning_effort").is_none());
}

#[tokio::test]
async fn the_default_request_without_a_profile_is_unchanged() {
    let mut input = request(None, layout());
    input.reasoning_effort = Some(ReasoningEffort::High);
    input.temperature = Some(0.2);
    let sent = send(input, "ok").await;
    assert_eq!(
        sent.body,
        json!({
            "model": "qwen3.8-27b",
            "messages": [
                {"role": "system", "content": "policy"},
                {"role": "system", "content": "rules"},
                {"role": "system", "content": "memory"},
                {"role": "user", "content": "hi"},
            ],
            "stream": true,
            "stream_options": {"include_usage": true},
            "reasoning_effort": "high",
            "temperature": 0.2,
            "tools": [{"type": "function", "function": {
                "name": "read_file", "description": "d", "parameters": {"type": "object"}}}],
            "tool_choice": "auto",
        })
    );
}

#[tokio::test]
async fn the_openai_profile_keeps_the_effort_field_and_may_force_a_tool() {
    let profile = Arc::new(builtin_profile(OPENAI_PROFILE_ID).unwrap());
    let mut input = request(Some(profile), layout());
    input.reasoning_effort = Some(ReasoningEffort::Low);
    input.tool_choice = ToolChoice::Required;
    let sent = send(input, "ok").await;
    assert_eq!(sent.body["reasoning_effort"], "low");
    assert_eq!(sent.body["tool_choice"], "required");
    assert_eq!(sent.body["messages"].as_array().unwrap().len(), 4);
    assert!(sent.body.get("chat_template_kwargs").is_none());
    assert!(warnings(&sent).is_empty());
}

#[tokio::test]
async fn an_auto_only_profile_downgrades_a_forced_tool_and_warns() {
    let mut input = request(Some(qwen()), layout());
    input.tool_choice = ToolChoice::Named("read_file".into());
    let sent = send(input, "ok").await;
    assert_eq!(sent.body["tool_choice"], "auto");
    assert_eq!(warnings(&sent), ["tool_choice_downgraded"]);
}

fn two_turn_history() -> Vec<ChatMessage> {
    let mut messages = layout();
    let mut assistant = ChatMessage::text("assistant", "");
    assistant.tool_calls = vec![ToolCall {
        id: "c1".into(),
        name: "read_file".into(),
        arguments_json: r#"{"file_path":"a"}"#.into(),
    }];
    let mut metadata = serde_json::Map::new();
    metadata.insert(REASONING_METADATA_KEY.into(), "I should read a.".into());
    assistant.metadata = Some(metadata);
    messages.push(assistant);
    let mut result = ChatMessage::text("tool", "{\"ok\":true}");
    result.tool_call_id = Some("c1".into());
    result.name = Some("read_file".into());
    messages.push(result);
    messages
}

#[tokio::test]
async fn reasoning_is_sent_back_on_the_earlier_assistant_message() {
    let sent = send(request(Some(qwen()), two_turn_history()), "ok").await;
    let assistant = &sent.body["messages"][2];
    assert_eq!(assistant["role"], "assistant");
    assert_eq!(assistant["reasoning_content"], "I should read a.");
    assert_eq!(assistant["tool_calls"][0]["id"], "c1");
    // The later message has none to send.
    assert!(sent.body["messages"][3].get("reasoning_content").is_none());
}

#[tokio::test]
async fn reasoning_stays_home_when_the_profile_does_not_ask_for_it() {
    let mut profile = builtin_profile(QWEN_PROFILE_ID).unwrap();
    profile.reasoning.send_back = false;
    let sent = send(request(Some(Arc::new(profile)), two_turn_history()), "ok").await;
    assert!(sent.body["messages"][2].get("reasoning_content").is_none());
    // Without any profile, as before.
    let plain = send(request(None, two_turn_history()), "ok").await;
    assert!(plain.body["messages"][4].get("reasoning_content").is_none());
}

#[tokio::test]
async fn malformed_history_arguments_become_an_empty_object_and_are_recorded() {
    let mut messages = layout();
    let mut assistant = ChatMessage::text("assistant", "");
    assistant.tool_calls = ["{\"a\":", "[1,2]", "{\"ok\":1}"]
        .iter()
        .enumerate()
        .map(|(index, arguments)| ToolCall {
            id: format!("c{index}"),
            name: "read_file".into(),
            arguments_json: (*arguments).into(),
        })
        .collect();
    messages.push(assistant);
    for index in 0..3 {
        let mut result = ChatMessage::text("tool", "{}");
        result.tool_call_id = Some(format!("c{index}"));
        messages.push(result);
    }
    // The sanitiser is not a profile feature: it runs without one too.
    for profile in [Some(qwen()), None] {
        let sent = send(request(profile, messages.clone()), "ok").await;
        let calls = sent.body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message.get("tool_calls").is_some())
            .unwrap()["tool_calls"]
            .clone();
        let arguments: Vec<_> = calls
            .as_array()
            .unwrap()
            .iter()
            .map(|call| call["function"]["arguments"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(arguments, ["{}", "{}", "{\"ok\":1}"]);
        assert_eq!(warnings(&sent), ["history_arguments_sanitized"]);
    }
}

const XML_CALL: &str = "Reading it.\n<tool_call>\n<function=read_file>\n<parameter=file_path>\nsrc/a.rs\n</parameter>\n</function>\n</tool_call>";

fn completed(sent: &Sent) -> Value {
    sent.events
        .iter()
        .find_map(|event| match event {
            ProviderEvent::Event { kind, data } if kind == "run.message.completed" => {
                Some(data.clone())
            }
            _ => None,
        })
        .unwrap()
}
fn requested(sent: &Sent) -> Vec<ToolCall> {
    sent.events
        .iter()
        .filter_map(|event| match event {
            ProviderEvent::ToolRequested(call) => Some(call.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn unparsed_xml_calls_are_salvaged_once_when_the_profile_asks() {
    let sent = send(request(Some(qwen()), layout()), XML_CALL).await;
    let calls = requested(&sent);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].name, "read_file");
    assert_eq!(
        serde_json::from_str::<Value>(&calls[0].arguments_json).unwrap(),
        json!({"file_path": "src/a.rs"})
    );
    assert_eq!(completed(&sent)["content"], "Reading it.");
    assert_eq!(completed(&sent)["toolCallCount"], 1);
    assert_eq!(warnings(&sent), ["tool_call_salvaged"]);
}

#[tokio::test]
async fn nothing_is_salvaged_with_the_option_off_or_when_the_server_parsed_calls() {
    let mut off = builtin_profile(QWEN_PROFILE_ID).unwrap();
    off.recovery.salvage = false;
    let sent = send(request(Some(Arc::new(off)), layout()), XML_CALL).await;
    assert!(requested(&sent).is_empty());
    assert_eq!(completed(&sent)["content"], XML_CALL);
    assert!(warnings(&sent).is_empty());
    // No profile at all: same.
    let sent = send(request(None, layout()), XML_CALL).await;
    assert!(requested(&sent).is_empty());
    // A request without tools has nothing to salvage into.
    let mut bare = request(Some(qwen()), layout());
    bare.tools.clear();
    assert!(requested(&send(bare, XML_CALL).await).is_empty());
}

// ---- the Responses lane shares the history sanitiser and the choice policy -

fn responses_identity() -> ConnectionIdentity {
    ConnectionIdentity {
        provider_id: "provider".into(),
        connection_id: "account".into(),
        protocol: "responses".into(),
        generation: 1,
    }
}

fn responses_history() -> Vec<ChatMessage> {
    let mut assistant = ChatMessage::text("assistant", "");
    assistant.tool_calls = ["{\"a\":", "[1,2]", "{\"ok\":1}"]
        .iter()
        .enumerate()
        .map(|(index, arguments)| ToolCall {
            id: format!("c{index}"),
            name: "read_file".into(),
            arguments_json: (*arguments).into(),
        })
        .collect();
    let mut messages = vec![ChatMessage::text("user", "go"), assistant];
    for index in 0..3 {
        let mut result = ChatMessage::text("tool", "{}");
        result.tool_call_id = Some(format!("c{index}"));
        messages.push(result);
    }
    messages
}

#[test]
fn responses_history_arguments_are_sanitised_and_recorded() {
    for profile in [Some(qwen()), None] {
        let input = request(profile, responses_history());
        let (body, _, notes) =
            build_responses_request_notes(&input, &responses_identity(), false).unwrap();
        let arguments: Vec<&str> = body["input"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|item| item["type"] == "function_call")
            .map(|item| item["arguments"].as_str().unwrap())
            .collect();
        assert_eq!(arguments, ["{}", "{}", "{\"ok\":1}"]);
        let codes: Vec<&str> = notes.iter().map(|note| note.code).collect();
        assert_eq!(codes, ["history_arguments_sanitized"]);
    }
}

#[test]
fn responses_tool_choice_follows_the_profile_policy() {
    let go = || vec![ChatMessage::text("user", "go")];
    // Auto is the default and stays unwritten.
    let input = request(Some(qwen()), go());
    let (body, _, notes) =
        build_responses_request_notes(&input, &responses_identity(), false).unwrap();
    assert!(body.get("tool_choice").is_none() && notes.is_empty());
    // An auto-only profile downgrades a forced tool and says so.
    let mut input = request(Some(qwen()), go());
    input.tool_choice = ToolChoice::Named("read_file".into());
    let (body, _, notes) =
        build_responses_request_notes(&input, &responses_identity(), false).unwrap();
    assert_eq!(body["tool_choice"], "auto");
    assert_eq!(
        notes.iter().map(|n| n.code).collect::<Vec<_>>(),
        ["tool_choice_downgraded"]
    );
    // A profile that allows it, and no profile, send what was asked.
    let openai = Arc::new(builtin_profile(OPENAI_PROFILE_ID).unwrap());
    for profile in [Some(openai), None] {
        let mut input = request(profile, go());
        input.tool_choice = ToolChoice::Required;
        let (body, _, notes) =
            build_responses_request_notes(&input, &responses_identity(), false).unwrap();
        assert_eq!(body["tool_choice"], "required");
        assert!(notes.is_empty());
        input.tool_choice = ToolChoice::Named("read_file".into());
        let (body, _, _) =
            build_responses_request_notes(&input, &responses_identity(), false).unwrap();
        assert_eq!(
            body["tool_choice"],
            json!({"type": "function", "name": "read_file"})
        );
    }
}
