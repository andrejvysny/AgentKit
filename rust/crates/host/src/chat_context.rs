use crate::{ExecutionContext, HostError, HostResult};
use agentkit_core::{ChatMessage, ChatRequest, CoreError};
use serde_json::{Value, json};

#[derive(Clone)]
pub(crate) struct ContextBudget {
    pub(super) window: u64,
    source: &'static str,
    pub(super) reserve: u64,
}

/// A native connection snapshot binds model metadata independently of live catalogs.
fn frozen_provider_window(payload: &Value, model: &str) -> HostResult<Option<u64>> {
    let snapshot = &payload["providerSnapshot"];
    if snapshot["model"].as_str() != Some(model) {
        return Ok(None);
    }
    let window = snapshot["connection"]["config"]["models"]
        .as_array()
        .and_then(|models| models.iter().find(|row| row["id"].as_str() == Some(model)))
        .and_then(|row| row.get("context_window_tokens"))
        .filter(|value| !value.is_null());
    window
        .map(|value| {
            value
                .as_u64()
                .filter(|value| *value > 0 && *value <= 9_007_199_254_740_991)
                .ok_or_else(|| HostError::Execution("invalid_provider_context_window".into()))
        })
        .transpose()
}

/// The chat's session descriptor with THIS turn's staged `sessionContext`
/// snapshot overlaid — the single place the task payload is allowed to
/// replace the live descriptor, shared by the budget and the context
/// assembly so a turn can never plan against a snapshot the other half
/// rejected. A snapshot outside the descriptor grammar fails with the
/// stable `session_context_invalid` refusal (an explicit null removes the
/// context, exactly as the staged absence means).
pub(crate) async fn effective_session_descriptor(
    context: &ExecutionContext,
    chat_id: &str,
) -> HostResult<Option<Value>> {
    if let Some(snapshot) = context.task.payload.get("sessionDescriptor") {
        let metadata = json!({"agentkit.session": snapshot});
        return crate::read_session_descriptor(&metadata)
            .map(Some)
            .ok_or_else(|| HostError::Execution("session_context_invalid".into()));
    }
    let descriptor = context
        .store
        .get_chat(chat_id)
        .await?
        .and_then(|chat| crate::read_session_descriptor(&chat.metadata));
    let descriptor = match descriptor {
        Some(descriptor) => descriptor,
        None if context.task.payload.get("sessionContext").is_some() => {
            json!({"version":1,"profile":{"id":"frozen"},"createdAt":"1970-01-01T00:00:00.000Z"})
        }
        None => return Ok(None),
    };
    crate::context_window::with_snapshot(descriptor, context.task.payload.get("sessionContext"))
        .map(Some)
        .ok_or_else(|| HostError::Execution("session_context_invalid".into()))
}

/// What the provider-facing history is assembled from (ADR 0017 part 1):
/// the anchored-history policy and the tool-result elision for THIS turn,
/// both resolved exactly once from the staged snapshot.
pub(crate) struct ContextAssembly {
    pub(crate) policy: super::history::HistoryPolicy,
    pub(crate) elision: Option<super::elision::ToolResultElisionOptions>,
}

/// The frozen session defaults (`turn-runner.ts`): the policy defaults to
/// `transcript` — the native executor always replayed the active-path
/// transcript — and tool-result elision is opt-in. A chat that is not a
/// session assembles as it always did: full replay, no elision.
pub(crate) async fn resolve_context_assembly(
    context: &ExecutionContext,
    chat_id: &str,
) -> HostResult<ContextAssembly> {
    let descriptor = effective_session_descriptor(context, chat_id).await?;
    let options = descriptor
        .as_ref()
        .and_then(|descriptor| descriptor.get("context"));
    Ok(ContextAssembly {
        policy: match options
            .and_then(|context| context.get("policy"))
            .and_then(Value::as_str)
        {
            Some("final_answers") => super::history::HistoryPolicy::FinalAnswers,
            // The descriptor grammar admits only `final_answers`/`transcript`;
            // absent (or a non-session chat) is the frozen default.
            _ => super::history::HistoryPolicy::Transcript,
        },
        elision: options
            .and_then(|context| context.get("toolResultElision"))
            .and_then(|value| super::elision::resolve_tool_result_elision(Some(value))),
    })
}

impl ContextBudget {
    pub(crate) async fn resolve(
        context: &ExecutionContext,
        chat_id: &str,
        provider: &str,
        model: &str,
    ) -> HostResult<Option<Self>> {
        let descriptor = effective_session_descriptor(context, chat_id)
            .await?
            .unwrap_or_else(|| json!({}));
        let (window, source) = if let Some(snapshot) =
            context.task.payload.get("contextWindowTokens")
        {
            (
                snapshot
                    .as_u64()
                    .filter(|value| *value > 0 && *value <= 9_007_199_254_740_991)
                    .ok_or_else(|| {
                        HostError::Execution("invalid_context_window_snapshot".into())
                    })?,
                // A frozen per-turn override is part of the admitted descriptor authority.
                "descriptor",
            )
        } else if descriptor["context"]["windowTokens"].as_u64().is_none()
            && let Some(window) = frozen_provider_window(&context.task.payload, model)?
        {
            (window, "model")
        } else {
            crate::context_window::resolve(&context.store, &descriptor, Some(provider), Some(model))
                .await?
        };
        let reserve = descriptor["context"]["reserveTokens"]
            .as_u64()
            .filter(|n| *n > 0)
            .unwrap_or(16384)
            .min(window / 2);
        Ok(Some(Self {
            window,
            source,
            reserve,
        }))
    }

    pub(crate) fn admit(
        &self,
        input: &mut ChatRequest,
        request_id: String,
        implicit_limit: bool,
    ) -> Result<Value, CoreError> {
        if input
            .max_output_tokens
            .is_some_and(|value| value == 0 || value > 9_007_199_254_740_991)
        {
            return Err(CoreError::new(
                "invalid_max_output_tokens",
                "maxOutputTokens must be a positive safe integer.",
            ));
        }
        let messages = input
            .selected_content
            .iter()
            .chain(input.messages.iter())
            .map(message_tokens)
            .sum::<u64>();
        let tools = if input.tools.is_empty() {
            0
        } else {
            let serialized = serde_json::to_string(&input.tools).map_err(|_| {
                CoreError::new("context_invalid", "Cannot estimate tool definitions.")
            })?;
            chars(&serialized).div_ceil(4)
        };
        let estimated = messages.saturating_add(tools);
        let budget = self.window as i128 - self.reserve as i128 - tools as i128;
        let available = self.window.saturating_sub(estimated);
        let output = input
            .max_output_tokens
            .unwrap_or(self.reserve)
            .min(self.reserve)
            .min(available);
        if i128::from(messages) > budget || output == 0 {
            return Err(CoreError::new(
                "context_overflow",
                &format!(
                    "The current request needs about {messages} message tokens but only {budget} fit in the {}-token window after the reply reserve, tool definitions and run content. Shorten the request or compact the session.",
                    self.window
                ),
            ));
        }
        let forwards_limit = input.max_output_tokens.is_some() || implicit_limit;
        if forwards_limit {
            input.max_output_tokens = Some(output);
        }
        let mut admission = json!({"requestId": request_id, "windowTokens": self.window, "windowSource": self.source,
            "reserveTokens": self.reserve, "toolTokens": tools, "extraTokens": 0,
            "estimatedInputTokens": estimated, "budgetTokens": budget as i64});
        if forwards_limit {
            admission["maxOutputTokens"] = output.into();
        }
        Ok(admission)
    }
}

fn chars(text: &str) -> u64 {
    text.encode_utf16().count() as u64
}
pub(super) fn message_tokens(message: &ChatMessage) -> u64 {
    let content = match &message.content {
        Value::String(text) => chars(text),
        Value::Array(parts) => parts
            .iter()
            .map(|part| match part["type"].as_str() {
                Some("text") => part["text"].as_str().map(chars).unwrap_or(0),
                Some("image") => 4800,
                _ => part["data"].as_str().map(chars).unwrap_or(0),
            })
            .sum(),
        other => chars(&other.to_string()),
    };
    (content
        + message
            .tool_calls
            .iter()
            .map(|call| chars(&call.name) + chars(&call.arguments_json))
            .sum::<u64>())
    .div_ceil(4)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentkit_store::{ClaimInput, SqliteStore, SubmitTask};
    use std::sync::Arc;
    use tokio::sync::Mutex;
    use tokio_util::sync::CancellationToken;

    struct Clock;
    impl crate::Runtime for Clock {
        fn now_ms(&self) -> i64 {
            1000
        }
        fn id(&self) -> String {
            "test".into()
        }
    }

    /// A context over an in-memory store carrying one session chat; resolves
    /// the budget exactly as the turn executor does.
    async fn fixture(metadata: Value) -> (SqliteStore, ExecutionContext) {
        let store = SqliteStore::memory(2).unwrap();
        store
            .create_chat("chat", None, metadata, 1000)
            .await
            .unwrap();
        store
            .submit(SubmitTask {
                task_id: "run".into(),
                kind: "chat.turn".into(),
                scope_id: "chat".into(),
                payload: json!({}),
                priority: 0,
                now: 1000,
                available_at: None,
                depends_on: vec![],
                parent_task_id: None,
                exclusive_scope: false,
            })
            .await
            .unwrap();
        let claimed = store
            .claim(ClaimInput {
                owner_id: "worker".into(),
                now: 1000,
                ttl_ms: 60_000,
                kinds: vec![],
                busy_scopes: vec![],
                max_attempts: 3,
            })
            .await
            .unwrap()
            .unwrap();
        let context = ExecutionContext {
            task: claimed.task,
            attempt_id: claimed.lease.attempt_id.clone(),
            cancellation: CancellationToken::new(),
            interruption: CancellationToken::new(),
            suspension: Arc::new(Mutex::new(None)),
            store: store.clone(),
            fence: claimed.lease.fence(),
            runtime: Arc::new(Clock),
            event_gate: Arc::new(Mutex::new(())),
        };
        (store, context)
    }

    #[tokio::test]
    async fn frozen_native_model_window_precedes_catalog_with_explicit_task_and_session_bounds() {
        let (store, mut context) = fixture(session(None)).await;
        store
            .set_setting(
                "http.provider-catalog",
                "provider",
                json!([{"modelId":"model","contextWindowTokens":32768}]),
            )
            .await
            .unwrap();
        context.task.payload = json!({"providerSnapshot":{"model":"model","connection":{"config":{"models":[{"id":"model","context_window_tokens":256}]}}}});
        let budget = ContextBudget::resolve(&context, "chat", "provider", "model")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(budget.window, 256);
        assert_eq!(budget.source, "model");
        context.task.payload["sessionDescriptor"] =
            session(Some(json!({"windowTokens":512})))["agentkit.session"].clone();
        assert_eq!(
            ContextBudget::resolve(&context, "chat", "provider", "model")
                .await
                .unwrap()
                .unwrap()
                .window,
            512
        );
        context.task.payload["contextWindowTokens"] = json!(128);
        assert_eq!(
            ContextBudget::resolve(&context, "chat", "provider", "model")
                .await
                .unwrap()
                .unwrap()
                .window,
            128
        );
    }

    #[tokio::test]
    async fn frozen_context_windows_emit_contract_valid_admission_events() {
        let (_, mut context) = fixture(json!({})).await;
        for (payload, expected) in [
            (json!({"contextWindowTokens":256}), "descriptor"),
            (
                json!({"providerSnapshot":{"model":"model","connection":{"config":{"models":[{"id":"model","context_window_tokens":256}]}}}}),
                "model",
            ),
        ] {
            context.task.payload = payload;
            let budget = ContextBudget::resolve(&context, "chat", "provider", "model")
                .await
                .unwrap()
                .unwrap();
            let data = budget
                .admit(&mut ChatRequest::default(), "request".into(), true)
                .unwrap();
            let event = json!({"type":"run.context.admitted","runId":"run","timestamp":"2026-11-05T12:00:00.000Z","contractVersion":"0.7.0","eventId":"event","seq":0,"data":data});
            let event: agentkit_contracts::AiRunEvent = serde_json::from_value(event)
                .expect("the emitted admission event must satisfy the shared wire contract");
            let roundtrip = serde_json::to_value(event).unwrap();
            assert_eq!(roundtrip["data"]["windowSource"], expected);
            assert_eq!(roundtrip["data"]["windowTokens"].as_f64(), Some(256.0));
        }
    }

    fn session(context: Option<Value>) -> Value {
        let mut descriptor = json!({
            "version": 1,
            "profile": { "id": "p" },
            "createdAt": "2026-01-01T00:00:00.000Z",
        });
        if let Some(context) = context {
            descriptor["context"] = context;
        }
        json!({ "agentkit.session": descriptor })
    }

    fn model_row(model_id: &str, window: Option<u64>) -> Value {
        json!({
            "providerId": "p1",
            "modelId": model_id,
            "displayName": null,
            "fetchedAt": "2026-01-01T00:00:00.000Z",
            "contextWindowTokens": window,
        })
    }

    #[tokio::test]
    async fn stored_catalogue_window_decides_admission_not_the_fallback() {
        let (store, context) = fixture(session(None)).await;
        // The provider-store door (`providers.replaceModels`): a persisted
        // 64-token model must admit against 64, never the 32,768 fallback.
        store
            .set_setting(
                agentkit_store::PROVIDER_MODELS_NAMESPACE,
                "p1",
                json!([model_row("tiny", Some(64)), model_row("unlisted", None)]),
            )
            .await
            .unwrap();
        let budget = ContextBudget::resolve(&context, "chat", "p1", "tiny")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (budget.window, budget.source, budget.reserve),
            (64, "model", 32)
        );
        // A model NEITHER catalogue documents keeps the honest fallback.
        let budget = ContextBudget::resolve(&context, "chat", "p1", "unlisted")
            .await
            .unwrap()
            .unwrap();
        assert_eq!((budget.window, budget.source), (32768, "fallback"));
        // And the HTTP management plane's committed copy is honored too —
        // with the compat rows taking precedence when both exist.
        store
            .set_setting(
                "http.provider-catalog",
                "p1",
                json!([{"modelId": "tiny", "contextWindowTokens": 4096}]),
            )
            .await
            .unwrap();
        let budget = ContextBudget::resolve(&context, "chat", "p1", "tiny")
            .await
            .unwrap()
            .unwrap();
        assert_eq!((budget.window, budget.source), (64, "model"));
        store
            .set_setting(
                "http.provider-catalog",
                "p2",
                json!([{"modelId": "m", "contextWindowTokens": 256}]),
            )
            .await
            .unwrap();
        let budget = ContextBudget::resolve(&context, "chat", "p2", "m")
            .await
            .unwrap()
            .unwrap();
        assert_eq!((budget.window, budget.source), (256, "model"));
    }

    #[tokio::test]
    async fn descriptor_window_still_overrides_the_stored_catalogue() {
        let (store, context) = fixture(session(Some(json!({"windowTokens": 128})))).await;
        store
            .set_setting(
                agentkit_store::PROVIDER_MODELS_NAMESPACE,
                "p1",
                json!([model_row("tiny", Some(64))]),
            )
            .await
            .unwrap();
        let budget = ContextBudget::resolve(&context, "chat", "p1", "tiny")
            .await
            .unwrap()
            .unwrap();
        assert_eq!((budget.window, budget.source), (128, "descriptor"));
    }

    #[tokio::test]
    async fn budget_refuses_overflow_and_bounds_forwarded_output() {
        let budget = ContextBudget {
            window: 20,
            source: "descriptor",
            reserve: 5,
        };
        let mut request = ChatRequest {
            messages: vec![ChatMessage::text("user", &"x".repeat(64))],
            ..Default::default()
        };
        assert_eq!(
            budget
                .admit(&mut request, "one".into(), true)
                .unwrap_err()
                .code,
            "context_overflow"
        );
        request.messages[0] = ChatMessage::text("user", "12345678");
        request.max_output_tokens = Some(8);
        let event = budget.admit(&mut request, "two".into(), true).unwrap();
        assert_eq!(request.max_output_tokens, Some(5));
        assert_eq!(event["maxOutputTokens"], 5);
    }

    #[test]
    fn account_omits_implicit_limit_but_retains_explicit_limit() {
        let budget = ContextBudget {
            window: 20,
            source: "fallback",
            reserve: 5,
        };
        let mut request = ChatRequest::default();
        let event = budget.admit(&mut request, "one".into(), false).unwrap();
        assert_eq!(request.max_output_tokens, None);
        assert!(event.get("maxOutputTokens").is_none());
        request.max_output_tokens = Some(4);
        budget.admit(&mut request, "two".into(), false).unwrap();
        assert_eq!(request.max_output_tokens, Some(4));
    }
    #[test]
    fn selected_content_tools_and_reply_reserve_share_the_exact_estimate_boundary() {
        let mut request = ChatRequest { messages:vec![ChatMessage::text("user","abcd")],selected_content:vec![ChatMessage::text("system","skill")],tools:vec![serde_json::from_value(json!({"name":"t","version":"1","effect":"read","capability":"read","description":"","inputSchema":{}})).unwrap()],max_output_tokens:Some(99),..Default::default() };
        // UTF-16/4 estimates: canonical 1, selection 2, tool schema 25, reply 8.
        assert_eq!(serde_json::to_string(&request.tools).unwrap().len(), 98);
        let budget = ContextBudget {
            window: 36,
            source: "descriptor",
            reserve: 8,
        };
        assert!(budget.admit(&mut request, "fit".into(), true).is_ok());
        assert_eq!(request.max_output_tokens, Some(8));
        request.selected_content[0].content = "skillxxxx".into();
        assert_eq!(
            budget
                .admit(&mut request, "over".into(), true)
                .unwrap_err()
                .code,
            "context_overflow"
        );
        assert_eq!(request.messages[0].content, "abcd");
        assert_eq!(request.selected_content[0].content, "skillxxxx");
    }
    #[tokio::test]
    async fn admitted_context_survives_removed_live_session_metadata() {
        for full_snapshot in [true, false] {
            let metadata = session(Some(json!({"windowTokens":128,"reserveTokens":32})));
            let (store, mut context) = fixture(metadata.clone()).await;
            context.task.payload["sessionContext"] = json!({"windowTokens":128,"reserveTokens":32});
            if full_snapshot {
                context.task.payload["sessionDescriptor"] = metadata["agentkit.session"].clone();
            }
            store
                .mutate_chat("chat", 1001, |chat| {
                    chat.metadata = json!({});
                    Ok(())
                })
                .await
                .unwrap();
            let budget = ContextBudget::resolve(&context, "chat", "provider", "model")
                .await
                .unwrap()
                .unwrap();
            assert_eq!((budget.window, budget.reserve), (128, 32));
        }
    }
}
