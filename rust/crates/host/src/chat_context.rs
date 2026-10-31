use crate::{ExecutionContext, HostError, HostResult};
use agentkit_core::{ChatMessage, ChatRequest, CoreError};
use serde_json::{Value, json};

#[derive(Clone)]
pub(crate) struct ContextBudget {
    pub(super) window: u64,
    source: &'static str,
    pub(super) reserve: u64,
}

impl ContextBudget {
    pub(crate) async fn resolve(
        context: &ExecutionContext,
        chat_id: &str,
        provider: &str,
        model: &str,
    ) -> HostResult<Option<Self>> {
        let Some(chat) = context.store.get_chat(chat_id).await? else {
            return Ok(None);
        };
        let Some(mut descriptor) = crate::session::read_session_descriptor(&chat.metadata) else {
            return Ok(None);
        };
        if let Some(snapshot) = context.task.payload.get("sessionContext") {
            if snapshot.is_null() {
                descriptor.as_object_mut().unwrap().remove("context");
            } else {
                descriptor["context"] = snapshot.clone();
                descriptor = crate::session::read_session_descriptor(
                    &json!({"agentkit.session": descriptor}),
                )
                .ok_or_else(|| HostError::Execution("session_context_invalid".into()))?;
            }
        }
        let override_window = descriptor["context"]["windowTokens"]
            .as_u64()
            .filter(|n| *n > 0);
        let model_window = if override_window.is_none() {
            context.store.get_setting("http.provider-catalog", provider).await?
                .and_then(|catalog| catalog.as_array()?.iter().find(|item| item["modelId"].as_str() == Some(model))?["contextWindowTokens"].as_u64())
                .filter(|n| *n > 0)
        } else {
            None
        };
        let (window, source) = if let Some(value) = override_window {
            (value, "descriptor")
        } else if let Some(value) = model_window {
            (value, "model")
        } else {
            (32768, "fallback")
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
        let messages = input.messages.iter().map(message_tokens).sum::<u64>();
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

    #[test]
    fn budget_refuses_overflow_and_bounds_forwarded_output() {
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
}
