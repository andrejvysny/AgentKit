use crate::{DurableExecutionPort, EXECUTION_ID_METADATA_KEY};
use agentkit_core::{CoreError, Tool, ToolContext, ToolResult};
use serde_json::Value;
use std::sync::Arc;

pub(super) async fn execute_receipted(
    tool: Arc<dyn Tool>,
    durable: Option<Arc<dyn DurableExecutionPort>>,
    execution_id: Option<String>,
    mut context: ToolContext,
    input: Value,
) -> Result<ToolResult, CoreError> {
    // Only an admission names the execution: a caller-supplied value never
    // reaches the body.
    context.metadata.remove(EXECUTION_ID_METADATA_KEY);
    if let Some(id) = &execution_id {
        context.metadata.insert(
            EXECUTION_ID_METADATA_KEY.to_owned(),
            Value::from(id.clone()),
        );
    }
    let owns_start = tool.records_command_start();
    let completion = owns_start
        .then(|| {
            durable
                .as_ref()
                .map(|durable| durable.register_command_completion())
        })
        .flatten();
    let body = async move {
        let result = tool.execute(context, input).await;
        if let (Some(durable), Some(id), Ok(result)) = (&durable, execution_id.as_deref(), &result)
        {
            if owns_start && is_no_start(result) {
                durable.complete_not_started(id, result).await?;
            } else if owns_start && is_outcome_unknown(result) {
                // Not a result: the receipt stays admitted, which the ledger
                // reads as uncertain, so the same operation under any call
                // id asks the owner first (ADR 0038).
            } else {
                durable.complete(id, result).await?;
            }
        }
        result
    };
    if owns_start {
        // Dropping the harness's await must not discard a command's factual
        // no-start result or its durable completion.
        tokio::spawn(async move {
            let result = body.await;
            if let Some(completion) = completion {
                completion.finish(result.as_ref().err().map(|error| error.message.clone()));
            }
            result
        })
        .await
        .map_err(|_| {
            CoreError::new(
                "command_completion_failed",
                "Native command completion task did not settle; effect remains uncertain",
            )
        })?
    } else {
        body.await
    }
}

fn is_no_start(result: &ToolResult) -> bool {
    !result.ok
        && result.data.as_ref().is_some_and(|data| {
            data["errorCode"] == "command_not_started"
                && data["phase"] == "before_spawn"
                && data["started"] == false
        })
}

/// The driver of a command it started could not learn how it ended (a lost
/// ack, a timeout, an observe error): `tool_result_uncertain` with
/// `uncertain: true`. A proven end (a receipt, even one read during a
/// quit) is a result and never has this shape.
fn is_outcome_unknown(result: &ToolResult) -> bool {
    !result.ok
        && result.data.as_ref().is_some_and(|data| {
            data["errorCode"] == "tool_result_uncertain" && data["uncertain"] == true
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentkit_core::{CancellationToken, ToolDefinition, ToolLimits};
    use serde_json::json;
    use std::sync::Mutex;

    /// Records the execution id its body was handed.
    struct Capture {
        definition: ToolDefinition,
        seen: Mutex<Vec<Option<Value>>>,
    }

    #[async_trait::async_trait]
    impl Tool for Capture {
        fn definition(&self) -> &ToolDefinition {
            &self.definition
        }
        async fn execute(&self, context: ToolContext, _: Value) -> Result<ToolResult, CoreError> {
            let id = context.metadata.get(EXECUTION_ID_METADATA_KEY).cloned();
            self.seen.lock().unwrap().push(id);
            Ok(ToolResult {
                ok: true,
                data: None,
                sources: vec![],
                warnings: vec![],
                truncated: false,
                limits: context.limits,
                model_data: None,
                summary: None,
                status: None,
                model_content: None,
            })
        }
    }

    fn forged_context() -> ToolContext {
        let mut metadata = serde_json::Map::new();
        metadata.insert(EXECUTION_ID_METADATA_KEY.into(), json!("forged"));
        ToolContext {
            run_id: "run".into(),
            tool_call_id: Some("call-1".into()),
            attempt_id: None,
            chat_id: None,
            scope_id: None,
            user_id: None,
            bindings: vec![],
            limits: ToolLimits::default(),
            cancellation: CancellationToken::new(),
            dispatch_guard: None,
            metadata,
            suspension: None,
        }
    }

    #[tokio::test]
    async fn the_body_sees_only_the_admitted_execution_id() {
        let tool = Arc::new(Capture {
            definition: ToolDefinition {
                name: "mutate".into(),
                version: "1".into(),
                effect: "write".into(),
                capability: "test".into(),
                description: String::new(),
                input_schema: json!({"type": "object"}),
                output_schema: None,
                timeout_ms: None,
                examples: vec![],
            },
            seen: Mutex::new(vec![]),
        });
        let admitted = Some("exec-1".to_owned());
        execute_receipted(tool.clone(), None, admitted, forged_context(), json!({}))
            .await
            .unwrap();
        execute_receipted(tool.clone(), None, None, forged_context(), json!({}))
            .await
            .unwrap();
        assert_eq!(
            *tool.seen.lock().unwrap(),
            vec![Some(json!("exec-1")), None],
            "an admission names the execution; a caller-supplied value never reaches the body"
        );
    }
}
