use super::{
    content,
    transport::{self, read_json},
    ConnectionIdentity, TrustedTransport,
};
use crate::{
    sse::{SseDecoder, SseFrame},
    ChatProvider, ChatRequest, CoreError, ProviderEvent, ProviderStream, ToolCall,
};
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct ChatCompletionsProvider {
    transport: Arc<dyn TrustedTransport>,
    identity: ConnectionIdentity,
}
impl ChatCompletionsProvider {
    pub fn new(transport: Arc<dyn TrustedTransport>) -> Result<Self, CoreError> {
        let identity = transport.identity();
        if identity.provider_id.is_empty() || identity.connection_id.is_empty() {
            return Err(CoreError::new(
                "invalid_connection",
                "Invalid provider connection",
            ));
        }
        Ok(Self {
            transport,
            identity,
        })
    }
    async fn send(
        &self,
        input: &ChatRequest,
        mut body: Value,
        cancellation: CancellationToken,
    ) -> Result<reqwest::Response, CoreError> {
        for attempt in 0..2 {
            if cancellation.is_cancelled() {
                return Err(CoreError::new("cancelled", "aborted"));
            }
            self.transport.check_authority().await?;
            if self.transport.identity() != self.identity {
                return Err(CoreError::new(
                    "connection_changed",
                    "Provider connection changed",
                ));
            }
            if let Some(budget) = &input.budget {
                budget.consume("providerRequests").await?;
            }
            let response = tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(CoreError::new("cancelled", "aborted")),
                response = self.transport.request_guarded(
                    &self.identity,
                    "chat/completions",
                    Some(body.clone()),
                    input.request_guard.clone(),
                    cancellation.clone(),
                ) => response,
            };
            // The request never produced a response (DNS, refused connection,
            // TLS, a fetch double that threw): distinct from a provider that
            // answered with an error status. The transport codes genuine
            // network failures as `transport_error`; every other structured
            // verdict the transport raises (a revoked/rotated connection's
            // `connection_changed`, an `invalid_input` operation refusal)
            // IS the classification — the frozen TS client throws those
            // before the fetch, so collapsing them into `network_error`
            // here would misreport a revocation as an outage.
            let response = match response {
                Ok(response) => response,
                Err(_) if cancellation.is_cancelled() => {
                    return Err(CoreError::new("cancelled", "aborted"));
                }
                Err(error) if error.code == "transport_error" => {
                    return Err(CoreError::new("network_error", &error.message));
                }
                Err(error) => return Err(error),
            };
            if response.status().is_success() {
                // A response without a body can never carry an SSE stream; the
                // event contract distinguishes it from a stream that ends empty.
                if response.content_length() == Some(0) {
                    return Err(CoreError::new("empty_body", "Provider returned empty body"));
                }
                return Ok(response);
            }
            let status = response.status().as_u16();
            let detail = read_body(response, cancellation.clone()).await?;
            if attempt == 0
                && status == 400
                && body.get("max_tokens").is_some()
                && (detail.contains("max_tokens") || detail.contains("max_completion_tokens"))
            {
                let tokens = body
                    .as_object_mut()
                    .expect("request object")
                    .remove("max_tokens")
                    .expect("checked");
                body["max_completion_tokens"] = tokens;
            } else {
                return Err(CoreError::new(
                    &status.to_string(),
                    &format!("POST /chat/completions failed: {status}"),
                ));
            }
        }
        unreachable!("second request either returns or fails")
    }
    fn events(&self, input: ChatRequest, cancellation: CancellationToken) -> ProviderStream {
        let provider = self.clone();
        Box::pin(async_stream::try_stream! {
            let (body, roles) = build_body(&input)?;
            provider.transport.check_authority().await?;
            yield ProviderEvent::Started;
            if !roles.is_empty() {
                provider.transport.check_authority().await?;
                yield ProviderEvent::Warning {
                    code: "multimodal_flattened".into(),
                    message: format!(
                        "Flattened content parts to text on {} message(s); image parts were dropped.",
                        roles.join(", ")
                    ),
                };
            }
            let response = provider.send(&input, body, cancellation.clone()).await?;
            provider.transport.check_authority().await?;
            let mut bytes = response.bytes_stream();
            let mut parser = SseDecoder::new();
            let mut state = ChatState {
                call_id: input
                    .runtime
                    .as_ref()
                    .map(|runtime| runtime.id("call_"))
                    .unwrap_or_else(|| format!("call_{}", uuid::Uuid::new_v4())),
                ..ChatState::default()
            };
            let mut failure = None;
            loop {
                let next = tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => {
                        failure = Some(CoreError::new("cancelled", "aborted"));
                        None
                    },
                    next = bytes.next() => next,
                };
                provider.transport.check_authority().await?;
                let Some(chunk) = next else { break; };
                match chunk {
                    Ok(chunk) => match parser.push(&chunk) {
                        Ok(frames) => for frame in frames {
                            let accepted = provider
                                .transport
                                .check_authority().await
                                .and_then(|()| state.accept(frame));
                            match accepted {
                                Ok(events) => for event in events {
                                    provider.transport.check_authority().await?;
                                    yield event;
                                },
                                Err(error) => {
                                    failure = Some(error);
                                    break;
                                },
                            }
                        },
                        Err(error) => {
                            failure = Some(error);
                            break;
                        }
                    },
                    Err(_) => {
                        failure =
                            Some(CoreError::new("provider_error", "Provider stream failed"));
                        break;
                    }
                }
                if failure.is_some() {
                    break;
                }
            }
            if failure.is_none() {
                match parser.finish() {
                    Ok(frames) => for frame in frames {
                        let accepted = provider
                            .transport
                            .check_authority().await
                            .and_then(|()| state.accept(frame));
                        match accepted {
                            Ok(events) => for event in events {
                                provider.transport.check_authority().await?;
                                yield event;
                            },
                            Err(error) => {
                                failure = Some(error);
                                break;
                            },
                        }
                    },
                    Err(error) => failure = Some(error),
                }
            }
            provider.transport.check_authority().await?;
            let complete = failure.is_none()
                && (state.finish.is_some() || state.done)
                && !(state.dropped > 0 && !state.calls.is_empty());
            if let Some(usage) = &state.usage {
                provider.transport.check_authority().await?;
                yield ProviderEvent::Usage(
                    usage_data(usage, &input.model, &state.call_id, complete)?,
                );
            }
            if let Some(error) = failure {
                Err(error)?;
            }
            if !complete {
                Err(CoreError::new(
                    "incomplete_stream",
                    "Provider stream ended without a complete response or contained malformed tool frames.",
                ))?;
            }
            let mut calls = state.assemble();
            let duplicates = crate::tool_execution::dedupe(&mut calls);
            if !duplicates.is_empty() {
                provider.transport.check_authority().await?;
                yield ProviderEvent::Warning {
                    code: "duplicate_tool_call_id".into(),
                    message: format!(
                        "Provider reused tool call id(s) {} in one turn; the later call(s) were re-keyed so each stays answerable.",
                        duplicates.join(", ")
                    ),
                };
            }
            if state.finish.as_deref() == Some("tool_calls") && calls.is_empty() {
                provider.transport.check_authority().await?;
                yield ProviderEvent::Warning {
                    code: "tool_call_unparseable".into(),
                    message: "Provider reported finish_reason=tool_calls but emitted no usable tool call."
                        .into(),
                };
            }
            if state.dropped > 0 && state.content.is_empty() && calls.is_empty() {
                provider.transport.check_authority().await?;
                yield ProviderEvent::Warning {
                    code: "sse_parse".into(),
                    message: format!(
                        "Dropped {} malformed SSE line(s); response may be incomplete.",
                        state.dropped
                    ),
                };
            }
            let mut data = json!({
                "content": state.content,
                "toolCallCount": calls.len(),
                "toolCalls": calls,
            });
            if !state.reasoning.is_empty() {
                data["reasoningContent"] = state.reasoning.into();
            }
            if !state.citations.is_empty() {
                data["citations"] = state.citations.into();
            }
            if let Some(reason) = state.finish {
                data["finishReason"] = reason.into();
            }
            provider.transport.check_authority().await?;
            yield ProviderEvent::Event {
                kind: "run.message.completed".into(),
                data,
            };
            for call in calls {
                provider.transport.check_authority().await?;
                yield ProviderEvent::ToolRequested(call);
            }
        })
    }
}
#[async_trait]
impl ChatProvider for ChatCompletionsProvider {
    fn supports_usage_cleanup(&self) -> bool {
        true
    }
    fn connection_identity(&self) -> Option<ConnectionIdentity> {
        Some(self.identity.clone())
    }
    fn id(&self) -> &str {
        &self.identity.provider_id
    }
    fn tracks_transport_requests(&self) -> bool {
        true
    }
    fn stream_chat(&self, input: ChatRequest, cancellation: CancellationToken) -> ProviderStream {
        self.events(input, cancellation)
    }
    async fn list_models(&self, cancellation: CancellationToken) -> Result<Vec<Value>, CoreError> {
        let response = transport::dispatch(
            self.transport.as_ref(),
            &self.identity,
            "models",
            None,
            cancellation.clone(),
        )
        .await?;
        let payload = read_json(response, cancellation).await?;
        self.transport.check_authority().await?;
        let fetched = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        Ok(payload["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|model| {
                Some(json!({
                    "providerId": self.id(),
                    "modelId": model["id"].as_str()?,
                    "displayName": model.get("name").cloned().unwrap_or(Value::Null),
                    "fetchedAt": fetched,
                }))
            })
            .collect())
    }
}
fn build_body(input: &ChatRequest) -> Result<(Value, Vec<String>), CoreError> {
    content::validate_selected(&input.selected_content)?;
    let mut messages = vec![];
    let mut roles = vec![];
    for message in input.selected_content.iter().chain(input.messages.iter()) {
        let (content, dropped) = content::parts(message, false, true)?;
        if dropped && !roles.contains(&message.role) {
            roles.push(message.role.clone());
        }
        let mut mapped = json!({
            "role": message.role,
            "content": content,
        });
        if let Some(name) = &message.name {
            mapped["name"] = name.clone().into();
        }
        if let Some(id) = &message.tool_call_id {
            mapped["tool_call_id"] = id.clone().into();
        }
        if !message.tool_calls.is_empty() {
            mapped["tool_calls"] = message
                .tool_calls
                .iter()
                .map(|call| {
                    json!({
                        "id": call.id,
                        "type": "function",
                        "function": {
                            "name": call.name,
                            "arguments": call.arguments_json,
                        },
                    })
                })
                .collect();
        }
        messages.push(mapped);
    }
    let mut body = json!({
        "model": input.model,
        "messages": messages,
        "stream": true,
        "stream_options": {
            "include_usage": true,
        },
    });
    if let Some(value) = input.reasoning_effort {
        body["reasoning_effort"] = serde_json::to_value(value).expect("enum serialization");
    }
    if let Some(value) = input.temperature {
        body["temperature"] = value.into();
    }
    if let Some(value) = input.max_output_tokens {
        body["max_tokens"] = value.into();
    }
    if !input.tools.is_empty() {
        body["tools"] = input
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.input_schema,
                    },
                })
            })
            .collect();
        body["tool_choice"] = "auto".into();
    }
    Ok((body, roles))
}
async fn read_body(
    response: reqwest::Response,
    cancellation: CancellationToken,
) -> Result<String, CoreError> {
    let mut stream = response.bytes_stream();
    let mut bytes = vec![];
    while let Some(chunk) = tokio::select! {
        biased;
        _ = cancellation.cancelled() => return Err(CoreError::new("cancelled", "aborted")),
        next = stream.next() => next,
    } {
        let chunk =
            chunk.map_err(|_| CoreError::new("provider_error", "Provider response failed"))?;
        if bytes.len() + chunk.len() > 1024 * 1024 {
            return Err(CoreError::new(
                "response_limit_exceeded",
                "Provider response exceeds limit",
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    String::from_utf8(bytes)
        .map_err(|_| CoreError::new("provider_error", "Provider response invalid UTF-8"))
}
struct ChatState {
    content: String,
    reasoning: String,
    citations: Vec<Value>,
    citation_urls: std::collections::HashSet<String>,
    calls: Vec<ToolCall>,
    by_index: HashMap<u64, usize>,
    by_id: HashMap<String, usize>,
    finish: Option<String>,
    done: bool,
    dropped: usize,
    usage: Option<Value>,
    call_id: String,
    bytes: usize,
}
impl Default for ChatState {
    fn default() -> Self {
        Self {
            content: String::new(),
            reasoning: String::new(),
            citations: vec![],
            citation_urls: std::collections::HashSet::new(),
            calls: vec![],
            by_index: HashMap::new(),
            by_id: HashMap::new(),
            finish: None,
            done: false,
            dropped: 0,
            usage: None,
            call_id: format!("call_{}", uuid::Uuid::new_v4()),
            bytes: 0,
        }
    }
}
impl ChatState {
    fn accept(&mut self, frame: SseFrame) -> Result<Vec<ProviderEvent>, CoreError> {
        if frame.done {
            self.done = true;
            return Ok(vec![]);
        }
        self.bytes += frame.data.len();
        if self.bytes > 16 * 1024 * 1024 {
            return Err(CoreError::new(
                "stream_limit_exceeded",
                "Provider stream exceeds limit",
            ));
        }
        let Ok(value) = agentkit_contracts::parse_javascript_json(&frame.data) else {
            self.dropped += 1;
            return Ok(vec![]);
        };
        let mut events = vec![ProviderEvent::Activity];
        if value["usage"].is_object() {
            self.usage = Some(value["usage"].clone());
        }
        let choice = &value["choices"][0];
        let delta = &choice["delta"];
        if let Some(content) = delta["content"].as_str().filter(|text| !text.is_empty()) {
            self.content.push_str(content);
            events.push(ProviderEvent::Delta(content.into()));
        }
        // Reasoning models emit chain-of-thought into a separate field; stream it
        // incrementally (do NOT merge into the visible answer) so consumers can
        // follow the thinking while it arrives.
        if let Some(reasoning) = delta
            .get("reasoning_content")
            .or_else(|| delta.get("reasoning"))
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
        {
            self.reasoning.push_str(reasoning);
            events.push(ProviderEvent::Event {
                kind: "run.message.reasoning-delta".into(),
                data: json!({"delta": reasoning}),
            });
        }
        if let Some(calls) = delta["tool_calls"].as_array() {
            for call in calls {
                self.tool_delta(call)?;
            }
        }
        // OpenAI-compatible web-research lanes attest sources per chunk:
        // `annotations[].url_citation` (ChatGPT-style) and the plain
        // `citations` shape. Normalize both to the contract's `{url,title}`
        // and surface NEW ones on an additive `run.message.delta` frame
        // (`delta: ""`) so the durable log, SSE replay and the UI all see
        // them; the full set also rides the completed event.
        let fresh = self.accept_citations(&[&delta["annotations"], &delta["citations"]]);
        if !fresh.is_empty() {
            events.push(ProviderEvent::Event {
                kind: "run.message.delta".into(),
                data: json!({"delta": "", "citations": fresh}),
            });
        }
        if let Some(reason) = choice["finish_reason"]
            .as_str()
            .filter(|reason| !reason.is_empty())
        {
            self.finish = Some(reason.into());
        }
        Ok(events)
    }
    /// Fold every citation the chunk attests into the per-call set and
    /// return the ones not seen before. Accepted shapes: an `annotations`
    /// entry of `type: "url_citation"` (or untyped with a `url_citation`
    /// object) carrying `{url, title?}`, and the plain `citations` array of
    /// bare URL strings or `{url, title?}` objects. Anything malformed is
    /// dropped, never fatal — citations are decoration, not protocol.
    fn accept_citations(&mut self, fields: &[&Value]) -> Vec<Value> {
        let mut fresh = Vec::new();
        for field in fields {
            let Some(entries) = field.as_array() else {
                continue;
            };
            for entry in entries {
                let candidate = if let Some(inner) = entry.get("url_citation") {
                    if entry
                        .get("type")
                        .and_then(Value::as_str)
                        .is_some_and(|kind| kind != "url_citation")
                    {
                        continue;
                    }
                    inner
                } else {
                    entry
                };
                let citation = match candidate {
                    Value::String(url) if !url.is_empty() => json!({"url": url}),
                    Value::Object(_) => match candidate["url"].as_str() {
                        Some(url) if !url.is_empty() => {
                            match candidate["title"]
                                .as_str()
                                .filter(|title| !title.is_empty())
                            {
                                Some(title) => json!({"url": url, "title": title}),
                                None => json!({"url": url}),
                            }
                        }
                        _ => continue,
                    },
                    _ => continue,
                };
                let url = citation["url"].as_str().unwrap_or_default();
                if self.citation_urls.insert(url.to_owned()) {
                    self.citations.push(citation.clone());
                    fresh.push(citation);
                }
            }
        }
        fresh
    }
    fn tool_delta(&mut self, delta: &Value) -> Result<(), CoreError> {
        let id = delta["id"].as_str().unwrap_or("");
        let name = delta["function"]["name"].as_str().unwrap_or("");
        let index = agentkit_contracts::javascript_number(&delta["index"]).map(|number| {
            if number == 0.0 {
                0
            } else {
                number.to_bits()
            }
        });
        let same = |slot: &&usize| {
            let call = &self.calls[**slot];
            (id.is_empty() || call.id.is_empty() || call.id == id)
                && (name.is_empty() || call.name.is_empty() || call.name == name)
        };
        let existing = if let Some(index) = index {
            self.by_index.get(&index).filter(same).copied()
        } else if !id.is_empty() {
            self.by_id.get(id).copied()
        } else {
            self.calls.len().checked_sub(1).filter(|slot| {
                let call = &self.calls[*slot];
                name.is_empty() || call.name.is_empty() || call.name == name
            })
        };
        let slot = if let Some(slot) = existing {
            slot
        } else {
            if self.calls.len() >= 1024 {
                return Err(CoreError::new(
                    "stream_limit_exceeded",
                    "Too many provider tool calls",
                ));
            }
            self.calls.push(ToolCall {
                id: id.into(),
                name: String::new(),
                arguments_json: String::new(),
            });
            self.calls.len() - 1
        };
        let call = &mut self.calls[slot];
        if !id.is_empty() {
            call.id = id.into();
            self.by_id.insert(id.into(), slot);
        }
        if let Some(index) = index {
            self.by_index.insert(index, slot);
        }
        if !name.is_empty() {
            call.name = name.into();
        }
        if let Some(arguments) = delta["function"]["arguments"].as_str() {
            call.arguments_json.push_str(arguments);
        }
        Ok(())
    }
    fn assemble(&self) -> Vec<ToolCall> {
        self.calls
            .iter()
            .enumerate()
            .filter(|(_, call)| !call.name.is_empty())
            .map(|(index, call)| {
                let mut call = call.clone();
                if call.id.is_empty() {
                    call.id = format!("call_{index}");
                }
                if call.arguments_json.is_empty() {
                    call.arguments_json = "{}".into();
                }
                call
            })
            .collect()
    }
}
pub(crate) fn usage_data(
    usage: &Value,
    model: &str,
    call_id: &str,
    final_for_call: bool,
) -> Result<Value, CoreError> {
    let mut data = json!({
        "callId": call_id,
        "attempt": 1,
        "step": 0,
        "model": model,
        "source": "stream",
        "finalForCall": final_for_call,
    });
    for (source, target) in [
        ("prompt_tokens", "promptTokens"),
        ("completion_tokens", "completionTokens"),
        ("total_tokens", "totalTokens"),
    ] {
        if let Some(value) = usage.get(source) {
            if value
                .as_u64()
                .filter(|value| *value <= 9007199254740991)
                .is_none()
            {
                return Err(CoreError::responses("malformed_usage"));
            }
            data[target] = value.clone();
        }
    }
    // Servers send `null` for details they do not track; the event contract is
    // a number or absent, never null — non-numeric values are omitted exactly
    // as the TypeScript client does.
    for (details, key, target) in [
        (
            "prompt_tokens_details",
            "cached_tokens",
            "cachedPromptTokens",
        ),
        (
            "completion_tokens_details",
            "reasoning_tokens",
            "reasoningTokens",
        ),
    ] {
        if let Some(value) = usage
            .get(details)
            .and_then(Value::as_object)
            .and_then(|details| details.get(key))
            .filter(|value| value.is_number())
        {
            if value
                .as_u64()
                .filter(|value| *value <= 9007199254740991)
                .is_none()
            {
                return Err(CoreError::responses("malformed_usage"));
            }
            data[target] = value.clone();
        }
    }
    Ok(data)
}
