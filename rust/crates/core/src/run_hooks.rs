use super::State;
use crate::budget::{minimum, timeout};
use crate::{CoreError, ToolCall, ToolRegistry};
use serde_json::{Value, json};
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
        let per_hook = self.options.hook_timeout_ms.unwrap_or(5000);
        // The run's overall execution budget bounds hook waits too: a hook
        // that ignores its signal must never outlive the deadline the host
        // bought. The effective wait is the SMALLER of the per-hook deadline
        // (`hook_timeout_ms`; 0 disables that bound) and the budget's
        // remaining overall time; with neither bound the call runs free,
        // matching the frozen RACE-not-cancellation contract.
        let overall = self
            .options
            .request
            .budget
            .as_ref()
            .and_then(|budget| budget.remaining_ms());
        let wait = if per_hook == 0 {
            overall
        } else {
            minimum(Some(per_hook), overall)
        };
        tokio::select! {
            _ = self.options.cancellation.cancelled() => Outcome::Aborted,
            result = async {
                match wait {
                    Some(ms) => match tokio::time::timeout(Duration::from_millis(ms), call).await {
                        Err(_) => {
                            // Which bound expired? The overall deadline settles
                            // the run deadline-exceeded (the frozen budget
                            // aborts with `execution_timeout: overall`, and the
                            // abort-race makes every later checkpoint stop
                            // quietly), while the per-hook deadline is the
                            // ordinary hook timeout.
                            if self
                                .options
                                .request
                                .budget
                                .as_ref()
                                .and_then(|budget| budget.remaining_ms())
                                == Some(0)
                            {
                                if let Some(budget) = &self.options.request.budget {
                                    budget.fail(timeout("overall"));
                                }
                                self.options.cancellation.cancel();
                                Outcome::Aborted
                            } else {
                                Outcome::TimedOut
                            }
                        }
                        Ok(Err(_)) => Outcome::Failed,
                        Ok(Ok(value)) => Outcome::Value(value),
                    },
                    None => match call.await {
                        Err(_) => Outcome::Failed,
                        Ok(value) => Outcome::Value(value),
                    },
                }
            } => result,
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
            let envelope = match self
                .invoke_hook("beforeToolCall", index, context.clone())
                .await
            {
                Outcome::Aborted => return Ok(None),
                Outcome::Value(envelope) => envelope,
                _ => {
                    return Ok(Some(
                        "A loop hook could not authorize this tool call.".into(),
                    ));
                }
            };
            // Frozen decision table (`runToolGate` in loop-hooks.ts). The
            // bridge carries the verdict as `{"value": …, "defined": …}`
            // because JSON alone cannot tell `undefined` from `null`/`NaN`
            // (both stringify to `null`), and the frozen loop maps them to
            // OPPOSITE outcomes. Only an explicit allow — or no opinion at
            // all (`undefined`, i.e. `defined: false`) — passes. Anything
            // else denies, fail-closed on garbage: `null`, booleans, numbers
            // (NaN included), strings, arrays, objects without a plain
            // string `action`, and `action` values other than "allow".
            // A `deny` carries its `reason` only when it is a plain string;
            // any other reason shape falls back to the fixed gate sentence.
            // An envelope without a boolean `defined` flag is not an answer
            // at all — fail closed, exactly like a gate that threw.
            let Some(defined) = envelope.get("defined").and_then(Value::as_bool) else {
                return Ok(Some(
                    "A loop hook could not authorize this tool call.".into(),
                ));
            };
            if !defined {
                continue;
            }
            let decision = &envelope["value"];
            if decision["action"] == "allow" {
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
