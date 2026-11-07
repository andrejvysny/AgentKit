use crate::model::{ElicitResult, ElicitationAction};
use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

/// Explicit host interaction port. The application owns user consent and URL
/// presentation; merely receiving an elicitation never opens a browser.
#[async_trait]
pub trait ElicitationHandler: Send + Sync {
    async fn elicit(
        &self,
        request: serde_json::Value,
        cancellation: CancellationToken,
    ) -> Result<ElicitResult, rmcp::ErrorData>;
}

pub(crate) fn validate_result(
    request: &serde_json::Value,
    result: ElicitResult,
) -> Result<ElicitResult, rmcp::ErrorData> {
    let invalid = || {
        rmcp::ErrorData::invalid_params("MCP elicitation response does not match the request", None)
    };
    if result.action != ElicitationAction::Accept {
        return if result.content.is_none() {
            Ok(result)
        } else {
            Err(invalid())
        };
    }
    if request["mode"] == "url" {
        return if result.content.is_none() {
            Ok(result)
        } else {
            Err(invalid())
        };
    }
    let validator = agentkit_contracts::compile_tool_schema(&request["requestedSchema"])
        .map_err(|_| invalid())?;
    if !result
        .content
        .as_ref()
        .is_some_and(|content| validator.is_valid(content))
    {
        return Err(invalid());
    }
    Ok(result)
}
