use super::State;
use crate::{CoreError, ProviderEvent, ProviderStream, ToolContext};
use futures_util::{FutureExt, StreamExt};
use serde_json::{json, Value};
use std::sync::Arc;
impl State {
    /// Preserve already-observed billing without waiting on an uncooperative stream.
    pub(super) async fn flush_usage(
        &mut self,
        stream: &mut ProviderStream,
    ) -> Result<(), CoreError> {
        for _ in 0..64 {
            match stream.next().now_or_never() {
                Some(Some(Ok(ProviderEvent::Usage(mut data)))) => {
                    data["step"] = self.iteration.into();
                    self.emit("run.usage", data).await?;
                }
                Some(Some(Ok(_))) => {}
                _ => break,
            }
        }
        Ok(())
    }
}

impl State {
    pub(super) fn tool_context(
        &self,
        call: &crate::ToolCall,
        suspension: Option<Arc<crate::SuspendSlot>>,
    ) -> ToolContext {
        let mut metadata = self.options.tool_metadata.clone();
        metadata.insert(
            "toolEventId".into(),
            self.options.runtime.id("tool_").into(),
        );
        ToolContext {
            run_id: self.run_id.clone(),
            tool_call_id: Some(call.id.clone()),
            attempt_id: self.options.attempt_id.clone(),
            chat_id: self.options.chat_id.clone(),
            scope_id: self.options.scope_id.clone(),
            user_id: self.options.user_id.clone(),
            bindings: self.options.bindings.clone(),
            limits: self.options.limits.clone(),
            cancellation: self.options.cancellation.clone(),
            dispatch_guard: self.options.dispatch_guard.clone(),
            metadata,
            suspension,
        }
    }
}

impl State {
    pub(super) async fn park_suspension(
        &mut self,
        spec: &crate::HumanRequestSpec,
        call: &crate::ToolCall,
    ) -> Result<(), CoreError> {
        let mut data = json!({"requestId":spec.request_id,"toolCallId":call.id,"toolName":call.name,"step":self.iteration,"prompt":spec.content.get("prompt").and_then(Value::as_str).unwrap_or("")});
        for key in [
            "kind",
            "argsSha256",
            "approvalGeneration",
            "contextSha256",
            "requestedMode",
            "standing",
        ] {
            if let Some(value) = spec.content.get(key) {
                data[key] = value.clone();
            }
        }
        if let Some(expires) = &spec.expires_at {
            data["expiresAt"] = expires.clone().into();
        }
        self.emit("run.waiting", data).await?;
        let Some(sink) = self.options.suspender.clone() else {
            return Err(CoreError::new(
                "suspension_unavailable",
                "This host cannot park runs for human input; the request was refused.",
            ));
        };
        sink.park(spec, &call.id).await?;
        self.suspended = true;
        Ok(())
    }
    pub(super) async fn cancel_suspended(
        &mut self,
        call: &crate::ToolCall,
    ) -> Result<(), CoreError> {
        let outcome = crate::tool_execution::failed(
            call,
            &CoreError::new(
                "cancelled",
                "Run cancelled before this tool produced a result.",
            ),
            self.options.limits.max_bytes,
            json!({}),
        );
        let mut message = crate::ChatMessage::text("tool", &outcome.model_json);
        message.tool_call_id = Some(call.id.clone());
        message.name = Some(call.name.clone());
        self.pending_messages.push(message.clone());
        self.messages.push(message);
        self.emit(outcome.kind, outcome.data).await
    }
}
