//! The JS provider callback port.
//!
//! The JS side supplies the *final* assistant result for one request (the
//! JS wrapper awaits the user callback and resolves the bridge call). This
//! port synthesizes the durable provider event stream from it — `Started`,
//! one `Delta` for non-empty content, `ToolRequested` per call, optional
//! `Usage`, then `Completed`. Incremental chunk streaming from JS is a
//! follow-up; nothing here pretends events arrived incrementally.
//! A rejected JS callback is a provider failure (`CoreError`), never a
//! success and never a panic.

use std::sync::Arc;

use agentkit_core::CancellationToken;
use agentkit_core::{
    ChatProvider, ChatRequest, CoreError, ProviderEvent, ProviderStream, ToolCall,
};
use futures::stream::{self, StreamExt};
use serde_json::{Value, json};

use crate::jsbridge::{JsBridge, PendingKind};

pub(crate) struct JsProvider {
    bridge: Arc<JsBridge>,
}

impl JsProvider {
    pub(crate) fn new(bridge: Arc<JsBridge>) -> Self {
        Self { bridge }
    }
}

impl ChatProvider for JsProvider {
    fn id(&self) -> &str {
        "js"
    }

    fn stream_chat(&self, input: ChatRequest, cancellation: CancellationToken) -> ProviderStream {
        let bridge = Arc::clone(&self.bridge);
        Box::pin(
            stream::once(async move { deliver(&bridge, input, &cancellation).await })
                .flat_map(stream::iter),
        )
    }
}

fn failure(code: &str, message: &str) -> Vec<Result<ProviderEvent, CoreError>> {
    vec![Err(CoreError::new(code, message))]
}

async fn deliver(
    bridge: &Arc<JsBridge>,
    request: ChatRequest,
    cancellation: &CancellationToken,
) -> Vec<Result<ProviderEvent, CoreError>> {
    let call_id = uuid::Uuid::new_v4().to_string();
    let messages = match serde_json::to_value(&request.messages) {
        Ok(value) => value,
        Err(error) => {
            return failure(
                "invalid_input",
                &format!("chat messages were not serializable: {error}"),
            );
        }
    };
    let tools = match serde_json::to_value(&request.tools) {
        Ok(value) => value,
        Err(error) => {
            return failure(
                "invalid_input",
                &format!("tool definitions were not serializable: {error}"),
            );
        }
    };
    let payload = json!({
        "runId": request.run_id,
        "model": request.model,
        "messages": messages,
        "tools": tools,
        "temperature": request.temperature,
        "maxOutputTokens": request.max_output_tokens,
    });
    let result = match Arc::clone(bridge)
        .request(PendingKind::Provider, &call_id, &payload, cancellation)
        .await
    {
        Ok(value) => value,
        Err(message) => return failure("provider_callback_failed", &message),
    };
    let content = result
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let finish_reason = result
        .get("finishReason")
        .and_then(Value::as_str)
        .unwrap_or("stop")
        .to_owned();
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    if let Some(Value::Array(list)) = result.get("toolCalls") {
        for (index, entry) in list.iter().enumerate() {
            let Some(name) = entry.get("name").and_then(Value::as_str) else {
                return failure(
                    "invalid_input",
                    &format!("toolCalls[{index}] requires a string name"),
                );
            };
            let id = entry
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| format!("{call_id}-{index}"));
            let arguments_json = match entry.get("arguments") {
                Some(Value::String(text)) => text.clone(),
                Some(value) if !value.is_null() => {
                    serde_json::to_string(value).unwrap_or_else(|_| "{}".to_owned())
                }
                _ => "{}".to_owned(),
            };
            tool_calls.push(ToolCall {
                id,
                name: name.to_owned(),
                arguments_json,
            });
        }
    }
    let mut events = vec![Ok(ProviderEvent::Started)];
    if !content.is_empty() {
        events.push(Ok(ProviderEvent::Delta(content.clone())));
    }
    for call in tool_calls.clone() {
        events.push(Ok(ProviderEvent::ToolRequested(call)));
    }
    if let Some(usage) = result.get("usage").filter(|usage| usage.is_object()) {
        events.push(Ok(ProviderEvent::Usage(usage.clone())));
    }
    events.push(Ok(ProviderEvent::Completed {
        content,
        tool_calls,
        finish_reason,
    }));
    events
}
