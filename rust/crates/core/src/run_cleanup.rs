use super::State;
use crate::{CoreError, ProviderEvent, ProviderStream, ToolContext};
use futures_util::{FutureExt, StreamExt};
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
        let mut metadata = serde_json::Map::new();
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
