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
    /// The callback wraps an account-backed client (declared through
    /// `providerAuthKind`). Such adapters REJECT `temperature`/
    /// `maxOutputTokens` before any HTTP (`unsupported_parameter` in the TS
    /// reference `responses-request.ts`), so the lane must not inject an
    /// implicit output cap nor send options the adapter never declared.
    account_backed: bool,
}

impl JsProvider {
    pub(crate) fn new(bridge: Arc<JsBridge>) -> Self {
        Self {
            bridge,
            account_backed: false,
        }
    }
    pub(crate) fn with_account_backed(mut self, account_backed: bool) -> Self {
        self.account_backed = account_backed;
        self
    }
}

impl ChatProvider for JsProvider {
    fn id(&self) -> &str {
        "js"
    }

    /// TS parity (`bounded-context-client.ts`): a `chatgpt-account` client
    /// runs WITHOUT an enforced output ceiling unless the request itself
    /// carries one; the host may not stamp an implicit cap.
    fn allows_implicit_output_limit(&self) -> bool {
        !self.account_backed
    }

    fn stream_chat(&self, input: ChatRequest, cancellation: CancellationToken) -> ProviderStream {
        let bridge = Arc::clone(&self.bridge);
        let account_backed = self.account_backed;
        Box::pin(
            stream::once(
                async move { deliver(&bridge, input, &cancellation, account_backed).await },
            )
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
    account_backed: bool,
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
    let mut payload = json!({
        "runId": request.run_id,
        "model": request.model,
        "messages": messages,
        "tools": tools,
    });
    // The lane sends only what the adapter declared. An account-backed
    // callback rejects declared-but-absent options (a `null` reads as a
    // present parameter), so undeclared sampling/output parameters are
    // omitted outright; explicitly carried values still travel and the
    // adapter's verdict rules (the TS reference rejects them with
    // `unsupported_parameter` at request build). The API-key lane keeps
    // its verbatim payload.
    if !account_backed || request.temperature.is_some() {
        payload["temperature"] = request.temperature.into();
    }
    if !account_backed || request.max_output_tokens.is_some() {
        payload["maxOutputTokens"] = request.max_output_tokens.into();
    }
    if let Some(effort) = request.reasoning_effort {
        payload["reasoningEffort"] = serde_json::to_value(effort).expect("enum serialization");
    }
    if !request.selected_content.is_empty() {
        payload["runContent"] = json!({"messages":request.selected_content});
    }
    let result = match Arc::clone(bridge)
        .request_guarded(
            PendingKind::Provider,
            &call_id,
            &payload,
            request.request_guard.clone(),
            cancellation,
        )
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

/// Connector grants come only from the host callback, never the user request.
pub(crate) struct JsContentAuthority(pub Arc<JsBridge>);
#[async_trait::async_trait]
impl agentkit_sessions::ContentAuthority<agentkit_store::StoreError> for JsContentAuthority {
    async fn authorized_connectors(
        &self,
        chat_id: &str,
        cancel: CancellationToken,
    ) -> agentkit_sessions::SessionsResult<Vec<String>, agentkit_store::StoreError> {
        let value = self
            .0
            .clone()
            .request(
                PendingKind::ContentAuthority,
                &uuid::Uuid::new_v4().to_string(),
                &json!({"chatId":chat_id}),
                &cancel,
            )
            .await
            .map_err(agentkit_sessions::SessionsError::ContentAuthorityCallback)?;
        serde_json::from_value(value)
            .map_err(agentkit_sessions::SessionsError::InvalidContentAuthorityDetail)
    }
    async fn authority_identity(
        &self,
        chat_id: &str,
        required_connectors: &[String],
        cancel: CancellationToken,
    ) -> agentkit_sessions::SessionsResult<Value, agentkit_store::StoreError> {
        self.0.clone().request(PendingKind::ContentAuthority, &uuid::Uuid::new_v4().to_string(), &json!({"operation":"identity","chatId":chat_id,"requiredConnectors":required_connectors}), &cancel).await.map_err(agentkit_sessions::SessionsError::ContentAuthorityCallback)
    }
}
