use crate::{
    TrustedTransport,
    chat_completions::usage_data,
    responses_request::{build_responses_request_with_warnings, make_continuation},
    responses_state::{ResponsesResult, ResponsesState},
    sse::SseDecoder,
    transport::{dispatch, dispatch_status, failure, read_json},
};
use agentkit_core::{
    AuthKind, ChatProvider, ChatRequest, ConnectionIdentity, CoreError, ProviderEvent,
    ProviderStream,
};
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    sync::{Arc, Mutex, OnceLock, PoisonError},
};
use tokio_util::sync::CancellationToken;
#[derive(Clone)]
pub struct ResponsesProvider {
    transport: Arc<dyn TrustedTransport>,
    identity: ConnectionIdentity,
}
impl ResponsesProvider {
    pub fn new(transport: Arc<dyn TrustedTransport>) -> Result<Self, CoreError> {
        let identity = transport.identity();
        if identity.provider_id.is_empty()
            || identity.connection_id.is_empty()
            || identity.protocol != "responses"
        {
            return Err(CoreError::responses("invalid_connection"));
        }
        Ok(Self {
            transport,
            identity,
        })
    }
    fn events(&self, input: ChatRequest, cancellation: CancellationToken) -> ProviderStream {
        let provider = self.clone();
        Box::pin(async_stream::try_stream! {
            provider.transport.check_authority().await?;
            yield ProviderEvent::Started;
            if cancellation.is_cancelled() {
                Err(CoreError::new("cancelled", "aborted"))?;
            }
            let (body, warnings) = build_responses_request_with_warnings(
                &input,
                &provider.identity,
                provider.transport.auth_kind() == AuthKind::ChatGptAccount,
            )?;
            for message in warnings {
                provider.transport.check_authority().await?;
                yield ProviderEvent::Warning { code: "tool_schema_unsupported".into(), message };
            }
            let mut body = body;
            let response = provider.send(&input, &mut body, cancellation.clone()).await?;
            provider.transport.check_authority().await?;
            let mut bytes = response.bytes_stream();
            let mut decoder = SseDecoder::new();
            let mut state = ResponsesState::default();
            let mut emitted = String::new();
            let call_id = input
                .runtime
                .as_ref()
                .map(|runtime| runtime.id("call_"))
                .unwrap_or_else(|| format!("call_{}", uuid::Uuid::new_v4()));
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
                let frames = match chunk {
                    Ok(chunk) => decoder.push(&chunk),
                    Err(_) => Err(CoreError::responses("transport_error")),
                };
                match frames {
                    Ok(frames) => for frame in frames {
                        if let Err(error) = provider.transport.check_authority().await {
                            failure = Some(error);
                            break;
                        }
                        if frame.done {
                            continue;
                        }
                        let value = agentkit_contracts::parse_javascript_json(&frame.data)
                            .map_err(|_| CoreError::responses("malformed_response"));
                        match value.and_then(|value| state.accept(value)) {
                            Ok(delta) => {
                                provider.transport.check_authority().await?;
                                yield ProviderEvent::Activity;
                                if let Some(delta) = delta {
                                    emitted.push_str(&delta);
                                    provider.transport.check_authority().await?;
                                    yield ProviderEvent::Delta(delta);
                                }
                                if let Some(reasoning) = state.take_reasoning_delta() {
                                    provider.transport.check_authority().await?;
                                    yield ProviderEvent::Event {
                                        kind: "run.message.reasoning-delta".into(),
                                        data: json!({"delta": reasoning}),
                                    };
                                }
                                if let Some(progress) = state.take_tool_progress() {
                                    provider.transport.check_authority().await?;
                                    yield progress;
                                }
                            }
                            Err(error) => {
                                failure = Some(error);
                                break;
                            }
                        }
                    },
                    Err(error) => failure = Some(error),
                }
                if failure.is_some() {
                    break;
                }
            }
            if failure.is_none() {
                match decoder.finish() {
                    Ok(frames) => for frame in frames {
                        if !frame.done {
                            if let Err(error) = provider.transport.check_authority().await {
                                failure = Some(error);
                                break;
                            }
                            let value = agentkit_contracts::parse_javascript_json(&frame.data)
                                .map_err(|_| CoreError::responses("malformed_response"));
                            match value.and_then(|value| state.accept(value)) {
                                Ok(delta) => {
                                    provider.transport.check_authority().await?;
                                    yield ProviderEvent::Activity;
                                    if let Some(delta) = delta {
                                        emitted.push_str(&delta);
                                        provider.transport.check_authority().await?;
                                        yield ProviderEvent::Delta(delta);
                                    }
                                    if let Some(reasoning) = state.take_reasoning_delta() {
                                        provider.transport.check_authority().await?;
                                        yield ProviderEvent::Event {
                                            kind: "run.message.reasoning-delta".into(),
                                            data: json!({"delta": reasoning}),
                                        };
                                    }
                                    if let Some(progress) = state.take_tool_progress() {
                                        provider.transport.check_authority().await?;
                                        yield progress;
                                    }
                                }
                                Err(error) => {
                                    failure = Some(error);
                                    break;
                                }
                            }
                        }
                    },
                    Err(error) => failure = Some(error),
                }
            }
            let observed_usage = state.usage.clone();
            let outcome = if let Some(error) = failure {
                Err(error)
            } else {
                state.finish(&input.tools)
            };
            let result = match outcome {
                Ok(result) => result,
                Err(error) => {
                    if let Some(usage) = observed_usage
                        && let Ok(data) = response_usage(&usage, &input.model, &call_id, false) {
                            provider.transport.check_authority().await?;
                            yield ProviderEvent::Usage(data);
                        }
                    Err(error)?;
                    unreachable!()
                }
            };
            if let Some(usage) = &result.usage {
                response_usage(usage, &input.model, &call_id, true)?;
            }
            let persisted = provider
                .persist(&input, &body, &result, cancellation.clone())
                .await;
            match persisted {
                Ok(Some(state)) => yield ProviderEvent::Continuation(state),
                Ok(None) => {},
                Err(error) => {
                    if let Some(usage) = &result.usage
                        && let Ok(data) = response_usage(usage, &input.model, &call_id, false) {
                            provider.transport.check_authority().await?;
                            yield ProviderEvent::Usage(data);
                        }
                    Err(error)?;
                }
            }
            if result.content.len() > emitted.len() {
                provider.transport.check_authority().await?;
                yield ProviderEvent::Delta(result.content[emitted.len()..].into());
            }
            if let Some(usage) = &result.usage {
                provider.transport.check_authority().await?;
                yield ProviderEvent::Usage(response_usage(usage, &input.model, &call_id, true)?);
            }
            for call in &result.calls {
                provider.transport.check_authority().await?;
                yield ProviderEvent::ToolRequested(call.clone());
            }
            // Frozen `completionEvents` shape: `reasoningContent` rides on
            // the completed event ONLY when non-empty — the terminal
            // reconciliation preserved it (streamed prefix or terminal-only).
            let mut data = json!({
                "content": result.content,
                "toolCallCount": result.calls.len(),
                "toolCalls": result.calls,
            });
            if !result.reasoning.is_empty() {
                data["reasoningContent"] = result.reasoning.into();
            }
            data["finishReason"] = (if result.calls.is_empty() {
                "stop"
            } else {
                "tool_calls"
            })
            .into();
            provider.transport.check_authority().await?;
            yield ProviderEvent::Event {
                kind: "run.message.completed".into(),
                data,
            };
        })
    }
    /// Dispatch once; a 400 that blames the reasoning summary (unverified
    /// organization, model without summaries) is retried once without it,
    /// and the provider+model pair omits it for the rest of the process.
    async fn send(
        &self,
        input: &ChatRequest,
        body: &mut Value,
        cancellation: CancellationToken,
    ) -> Result<reqwest::Response, CoreError> {
        let key = (self.identity.provider_id.clone(), input.model.clone());
        if summary_rejected(&key) {
            strip_summary(body);
        }
        for attempt in 0..2 {
            if let Some(budget) = &input.budget {
                budget.consume("providerRequests").await?;
            }
            let outcome = dispatch_status(
                self.transport.as_ref(),
                &self.identity,
                "responses",
                Some(body.clone()),
                input.request_guard.clone(),
                cancellation.clone(),
            )
            .await?;
            let (status, payload) = match outcome {
                Ok(response) => return Ok(response),
                Err(rejection) => rejection,
            };
            let summary_only = body["reasoning"]
                .as_object()
                .is_some_and(|reasoning| reasoning.len() == 1 && reasoning.contains_key("summary"));
            if attempt == 0
                && status == 400
                && blames_summary(&payload, summary_only)
                && strip_summary(body)
            {
                remember_summary_rejection(key.clone());
                continue;
            }
            return Err(failure(status, &payload));
        }
        unreachable!("the retry either returns or fails")
    }
    async fn persist(
        &self,
        input: &ChatRequest,
        body: &Value,
        result: &ResponsesResult,
        cancellation: CancellationToken,
    ) -> Result<Option<Value>, CoreError> {
        self.transport.check_authority().await?;
        let has_reasoning = result.output.iter().any(|item| item["type"] == "reasoning");
        let continuation = make_continuation(
            input,
            &self.identity,
            body,
            &result.output,
            &result.content,
            &result.calls,
        )?;
        if let Some(continuation) = &continuation {
            if has_reasoning && input.continuation_store.is_none() {
                return Err(CoreError::responses("continuation_callback_required"));
            }
            if let Some(store) = &input.continuation_store {
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => return Err(CoreError::new(
                        "cancelled",
                        "aborted",
                    )),
                    result = store.save(continuation.clone()) => result?,
                }
            }
        }
        if cancellation.is_cancelled() {
            return Err(CoreError::new("cancelled", "aborted"));
        }
        self.transport.check_authority().await?;
        Ok(continuation)
    }
}
#[async_trait]
impl ChatProvider for ResponsesProvider {
    fn allows_implicit_output_limit(&self) -> bool {
        self.transport.auth_kind() != AuthKind::ChatGptAccount
    }
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
        let response = dispatch(
            self.transport.as_ref(),
            &self.identity,
            "models",
            None,
            cancellation.clone(),
        )
        .await?;
        let payload = read_json(response, cancellation).await?;
        self.transport.check_authority().await?;
        let account = self.transport.auth_kind() == AuthKind::ChatGptAccount;
        let models = payload[if account { "models" } else { "data" }]
            .as_array()
            .ok_or_else(|| CoreError::responses("malformed_model_catalog"))?;
        let mut output = vec![];
        let fetched = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        for model in models {
            if account && model["visibility"] != "list" {
                continue;
            }
            let id = model[if account { "slug" } else { "id" }]
                .as_str()
                .filter(|value| !value.is_empty())
                .ok_or_else(|| CoreError::responses("malformed_model_catalog"))?;
            let label = model.get(if account { "display_name" } else { "name" });
            if label.is_some_and(|value| !value.is_string()) {
                return Err(CoreError::responses("malformed_model_catalog"));
            }
            output.push(json!({
                "providerId": self.id(),
                "modelId": id,
                "displayName": label.cloned().unwrap_or(Value::Null),
                "fetchedAt": fetched,
            }));
        }
        Ok(output)
    }
}
/// Provider+model pairs whose provider rejected `reasoning.summary`. Process
/// memory only: a later process (or a newly verified organization) tries again.
fn summary_rejections() -> &'static Mutex<HashSet<(String, String)>> {
    static REJECTED: OnceLock<Mutex<HashSet<(String, String)>>> = OnceLock::new();
    REJECTED.get_or_init(Default::default)
}
fn summary_rejected(key: &(String, String)) -> bool {
    summary_rejections()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .contains(key)
}
fn remember_summary_rejection(key: (String, String)) {
    summary_rejections()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(key);
}
/// Removes `reasoning.summary` (and an emptied `reasoning`); true when the
/// body carried one.
fn strip_summary(body: &mut Value) -> bool {
    let Some(reasoning) = body.get_mut("reasoning").and_then(Value::as_object_mut) else {
        return false;
    };
    let removed = reasoning.remove("summary").is_some();
    if reasoning.is_empty() {
        body.as_object_mut().map(|body| body.remove("reasoning"));
    }
    removed
}
/// OpenAI names the parameter (`error.param`) or the feature in the message;
/// the ChatGPT backend answers `{detail}`. A summary mention counts; so does
/// any `reasoning` mention when the summary is all `reasoning` carries (a
/// model without reasoning support rejects the whole object). An error about
/// a requested effort is not the summary's fault.
fn blames_summary(payload: &Value, summary_only: bool) -> bool {
    let text = payload.to_string().to_ascii_lowercase();
    text.contains("summar") || (summary_only && text.contains("reasoning"))
}
fn response_usage(
    usage: &Value,
    model: &str,
    call_id: &str,
    final_for_call: bool,
) -> Result<Value, CoreError> {
    let mut mapped = json!({});
    for (source, target) in [
        ("input_tokens", "prompt_tokens"),
        ("output_tokens", "completion_tokens"),
        ("total_tokens", "total_tokens"),
        ("input_tokens_details", "prompt_tokens_details"),
        ("output_tokens_details", "completion_tokens_details"),
    ] {
        if let Some(value) = usage.get(source) {
            mapped[target] = value.clone();
        }
    }
    usage_data(&mapped, model, call_id, final_for_call)
}
