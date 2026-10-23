//! Fixture-only RPC for original tool contributor callbacks.
use agentkit_core::{CoreError, Tool, ToolContext, ToolDefinition, ToolResult};
use agentkit_host::{
    ContributedTool, HostError, HostResult, ToolContributionContext, ToolContributor,
};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::Arc;

pub struct Contributor {
    pub namespace: String,
    pub index: usize,
    pub url: String,
    pub token: String,
}
#[async_trait]
impl ToolContributor for Contributor {
    fn namespace(&self) -> &str {
        &self.namespace
    }
    async fn contribute(&self, ctx: &ToolContributionContext) -> HostResult<Vec<ContributedTool>> {
        let input = json!({"index":self.index,"context":{"chatId":ctx.chat_id,"runId":ctx.run_id,"scopeId":ctx.scope_id,"actorId":ctx.actor_id,"bindings":ctx.bindings,"limits":ctx.limits}});
        let request = reqwest::Client::new()
            .post(format!("{}/contribute", self.url))
            .bearer_auth(&self.token)
            .json(&input)
            .send();
        let response = tokio::select! {_=ctx.cancellation.cancelled()=>return Err(HostError::Cancelled),result=request=>result.map_err(error)?};
        let definitions: Vec<ToolDefinition> = response
            .error_for_status()
            .map_err(error)?
            .json()
            .await
            .map_err(error)?;
        Ok(definitions
            .into_iter()
            .map(|definition| {
                let effect = definition.effect.clone();
                let tool = Arc::new(CallbackTool {
                    definition,
                    url: self.url.clone(),
                    token: self.token.clone(),
                    index: self.index,
                }) as Arc<dyn Tool>;
                if effect == "read" {
                    ContributedTool::Read(tool)
                } else {
                    ContributedTool::DirectWrite(tool)
                }
            })
            .collect())
    }
}
struct CallbackTool {
    definition: ToolDefinition,
    url: String,
    token: String,
    index: usize,
}
#[async_trait]
impl Tool for CallbackTool {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }
    async fn execute(&self, ctx: ToolContext, input: Value) -> Result<ToolResult, CoreError> {
        let body = json!({"index":self.index,"name":self.definition.name,"input":input,"context":{"runId":ctx.run_id,"chatId":ctx.chat_id,"scopeId":ctx.scope_id,"userId":ctx.user_id,"bindings":ctx.bindings,"limits":ctx.limits,"metadata":ctx.metadata}});
        let request = reqwest::Client::new()
            .post(format!("{}/tool", self.url))
            .bearer_auth(&self.token)
            .json(&body)
            .send();
        let response = tokio::select! {_=ctx.cancellation.cancelled()=>return Err(CoreError::new("aborted","Tool cancelled")),response=request=>response.map_err(core_error)?};
        response
            .error_for_status()
            .map_err(core_error)?
            .json()
            .await
            .map_err(core_error)
    }
}
fn error(error: impl std::fmt::Display) -> HostError {
    HostError::Execution(error.to_string())
}
fn core_error(error: impl std::fmt::Display) -> CoreError {
    CoreError::new("test_tool_bridge", &error.to_string())
}
