use super::State;
use crate::{CoreError, ToolCall, ToolRegistry};
use serde_json::{json, Value};
use std::time::Duration;

enum Outcome {
    Value(Value),
    Failed,
    TimedOut,
    Aborted,
}
impl State {
    async fn invoke_hook(&self, name: &str, index: usize, context: Value) -> Outcome {
        let Some(hooks) = &self.options.hooks else {
            return Outcome::Value(Value::Null);
        };
        let call = hooks.call(name, index, context, &self.options.cancellation);
        let deadline = self.options.hook_timeout_ms.unwrap_or(5000);
        tokio::select! {
            _ = self.options.cancellation.cancelled() => Outcome::Aborted,
            result = async { if deadline == 0 { Ok(call.await) } else { tokio::time::timeout(Duration::from_millis(deadline), call).await } } => match result {
                Err(_) => Outcome::TimedOut, Ok(Err(_)) => Outcome::Failed, Ok(Ok(value)) => Outcome::Value(value),
            }
        }
    }
    pub(super) async fn observe(&mut self, name: &str, context: Value) -> Result<(), CoreError> {
        let indexes = self
            .options
            .hooks
            .as_ref()
            .map(|hooks| hooks.indexes(name))
            .unwrap_or_default();
        for index in indexes {
            let failure = match self.invoke_hook(name, index, context.clone()).await {
                Outcome::Failed => Some(("hook_failed", "threw")),
                Outcome::TimedOut => Some(("hook_timeout", "timed out")),
                _ => None,
            };
            if let Some((code, detail)) = failure {
                self.warning(
                    code,
                    &format!("Loop hook {name}[{index}] {detail}; the run continued without it."),
                )
                .await?;
            }
        }
        Ok(())
    }
    pub(super) async fn tool_gate(
        &self,
        registry: &ToolRegistry,
        call: &ToolCall,
    ) -> Result<Option<String>, CoreError> {
        if registry.get(&call.name).is_none() {
            return Ok(None);
        }
        let Ok(args) = agentkit_contracts::parse_javascript_json(&call.arguments_json) else {
            return Ok(None);
        };
        if !registry.validate_input(&call.name, &args).is_empty() {
            return Ok(None);
        }
        let indexes = self
            .options
            .hooks
            .as_ref()
            .map(|hooks| hooks.indexes("beforeToolCall"))
            .unwrap_or_default();
        let context = json!({"runId":self.run_id,"step":self.iteration,"toolCallId":call.id,"toolName":call.name,"args":args});
        for index in indexes {
            let decision = match self
                .invoke_hook("beforeToolCall", index, context.clone())
                .await
            {
                Outcome::Aborted => return Ok(None),
                Outcome::Value(value) => value,
                _ => {
                    return Ok(Some(
                        "A loop hook could not authorize this tool call.".into(),
                    ))
                }
            };
            if decision.is_null() || decision["action"] == "allow" {
                continue;
            }
            let reason = if decision["action"] == "deny" {
                decision["reason"].as_str()
            } else {
                None
            };
            return Ok(Some(
                reason
                    .unwrap_or("A loop hook could not authorize this tool call.")
                    .into(),
            ));
        }
        Ok(None)
    }
}
