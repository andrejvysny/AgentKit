//! Provider wrappers: the recorder sees every request the production loop
//! sends (real tool definitions, frozen profile, system text); the scripted
//! model is the default, network-free stand-in for the live model.
use agentkit_core::{
    CacheCapability, ChatProvider, ChatRequest, ConnectionIdentity, Dialect, ProviderEvent,
    ProviderStream, ToolCall, ToolDefinition, ToolLane,
};
use agentkit_providers::conformance::Script;
use async_trait::async_trait;
use serde_json::Value;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
pub struct Capture {
    pub requests: usize,
    /// Tool definitions of the first request, as the model was offered them.
    pub tools: Vec<ToolDefinition>,
    /// System message text of the first request.
    pub system: Vec<String>,
    /// Profile id and the temperature the request carried, per request.
    pub profile_ids: Vec<Option<String>>,
    pub temperatures: Vec<Option<f64>>,
    /// Text of the last reply that carried no tool call.
    pub answer: String,
}

pub struct Recorder {
    inner: Arc<dyn ChatProvider>,
    capture: Arc<Mutex<Capture>>,
}

impl Recorder {
    pub fn new(inner: Arc<dyn ChatProvider>) -> (Arc<Self>, Arc<Mutex<Capture>>) {
        let capture = Arc::new(Mutex::new(Capture::default()));
        let recorder = Arc::new(Self {
            inner,
            capture: Arc::clone(&capture),
        });
        (recorder, capture)
    }
}

fn system_text(request: &ChatRequest) -> Vec<String> {
    request
        .messages
        .iter()
        .filter(|m| m.role == "system")
        .map(|m| match &m.content {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        })
        .collect()
}

#[async_trait]
impl ChatProvider for Recorder {
    fn allows_implicit_output_limit(&self) -> bool {
        self.inner.allows_implicit_output_limit()
    }
    fn apply_request_defaults(&self, input: &mut ChatRequest) {
        self.inner.apply_request_defaults(input);
    }
    fn supports_usage_cleanup(&self) -> bool {
        self.inner.supports_usage_cleanup()
    }
    fn connection_identity(&self) -> Option<ConnectionIdentity> {
        self.inner.connection_identity()
    }
    fn tracks_transport_requests(&self) -> bool {
        self.inner.tracks_transport_requests()
    }
    fn tool_lane(&self) -> ToolLane {
        self.inner.tool_lane()
    }
    fn cache_capability(&self) -> CacheCapability {
        self.inner.cache_capability()
    }
    fn id(&self) -> &str {
        self.inner.id()
    }

    fn stream_chat(&self, input: ChatRequest, cancellation: CancellationToken) -> ProviderStream {
        {
            let mut capture = self.capture.lock().expect("capture lock");
            if capture.requests == 0 {
                capture.tools = input.tools.clone();
                capture.system = system_text(&input);
            }
            capture.requests += 1;
            capture
                .profile_ids
                .push(input.profile.as_ref().map(|p| p.id.clone()));
            capture.temperatures.push(input.temperature);
        }
        let capture = Arc::clone(&self.capture);
        let stream = futures::StreamExt::inspect(
            self.inner.stream_chat(input, cancellation),
            move |event| match event {
                // Scripted models end a turn with `Completed`.
                Ok(ProviderEvent::Completed {
                    content,
                    tool_calls,
                    ..
                }) if tool_calls.is_empty() => {
                    capture.lock().expect("capture lock").answer = content.clone();
                }
                // Live Chat Completions and Responses providers end a turn with
                // a `run.message.completed` event instead; without this arm a
                // live run never records an answer and every scenario reports
                // "did not settle" regardless of what the model did.
                Ok(ProviderEvent::Event { kind, data })
                    if kind == "run.message.completed"
                        && data["toolCallCount"].as_u64().unwrap_or(0) == 0 =>
                {
                    if let Some(content) = data["content"].as_str() {
                        capture.lock().expect("capture lock").answer = content.to_owned();
                    }
                }
                _ => {}
            },
        );
        Box::pin(stream)
    }
}

/// How the scripted model behaves; the broken modes prove the assertions bite.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Behaviour {
    /// Follows the scenario script.
    Follow,
    /// Answers without calling a tool.
    NeverCalls,
}

/// A network-free model that plays one variant's script, one turn per request.
/// It speaks the session's dialect: a canonical scripted call is written the
/// way the model would see it (`wire_call`), so translation is exercised too.
pub struct ScriptedModel {
    script: Script,
    behaviour: Behaviour,
    dialect: Option<Dialect>,
    seen: AtomicUsize,
}

impl ScriptedModel {
    pub fn new(script: Script, behaviour: Behaviour, dialect: Option<Dialect>) -> Arc<Self> {
        Arc::new(Self {
            script,
            behaviour,
            dialect,
            seen: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl ChatProvider for ScriptedModel {
    fn id(&self) -> &str {
        "scripted"
    }

    fn stream_chat(&self, _input: ChatRequest, _cancellation: CancellationToken) -> ProviderStream {
        let turn = self.seen.fetch_add(1, Ordering::SeqCst);
        let calls = match self.behaviour {
            Behaviour::Follow => self.script.turns.get(turn).cloned().unwrap_or_default(),
            Behaviour::NeverCalls => vec![],
        };
        let tool_calls: Vec<ToolCall> = calls
            .iter()
            .enumerate()
            .map(|(i, call)| {
                let (name, args) = self
                    .dialect
                    .as_ref()
                    .and_then(|dialect| dialect.wire_call(&call.tool, &call.args))
                    .unwrap_or_else(|| (call.tool.clone(), call.args.clone()));
                ToolCall {
                    id: format!("call-{turn}-{i}"),
                    name,
                    arguments_json: args.to_string(),
                }
            })
            .collect();
        let (content, finish_reason) = if tool_calls.is_empty() {
            (self.script.answer.clone(), "stop")
        } else {
            (String::new(), "tool_calls")
        };
        Box::pin(futures::stream::iter([Ok(ProviderEvent::Completed {
            content,
            tool_calls,
            finish_reason: finish_reason.into(),
        })]))
    }
}
