use super::DurableExecution;
use agentkit_core::{CoreError, Tool, ToolContext, ToolResult};
use serde_json::Value;
use std::sync::Arc;

pub(super) async fn execute_receipted(
    tool: Arc<dyn Tool>,
    durable: Option<Arc<DurableExecution>>,
    execution_id: Option<String>,
    context: ToolContext,
    input: Value,
) -> Result<ToolResult, CoreError> {
    let owns_start = tool.records_command_start();
    let completion = owns_start
        .then(|| {
            durable.as_ref().map(|durable| {
                super::native_command_drain::CompletionGuard::register(&durable.context.store)
            })
        })
        .flatten();
    let body = async move {
        let result = tool.execute(context, input).await;
        if let (Some(durable), Some(id), Ok(result)) = (&durable, execution_id.as_deref(), &result)
        {
            if owns_start && is_no_start(result) {
                durable.complete_not_started(id, result).await?;
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
