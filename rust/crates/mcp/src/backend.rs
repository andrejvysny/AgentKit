use async_trait::async_trait;
use rmcp::{ErrorData, model::*};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug)]
pub struct SessionScope {
    pub principal: Value,
    pub chat_id: Option<String>,
    pub actor_id: String,
    pub cancellation: CancellationToken,
}

#[derive(Clone)]
pub struct CatalogTool {
    pub definition: Tool,
    /// Host-owned effect, never inferred from untrusted MCP annotations.
    pub writes: bool,
}

#[derive(Clone, Copy, Default)]
pub struct Features {
    pub resources: bool,
    pub prompts: bool,
}

/// Implementations must stage the same catalogue/guards used by the local host.
#[async_trait]
pub trait Backend: Send + Sync + 'static {
    fn features(&self) -> Features {
        Features::default()
    }
    async fn list_tools(&self, scope: &SessionScope) -> Result<Vec<CatalogTool>, ErrorData>;
    async fn call_tool(
        &self,
        name: &str,
        arguments: JsonObject,
        scope: &SessionScope,
    ) -> Result<CallToolResult, ErrorData>;
    /// Must revoke immediately; close paths invoke this before reporting success.
    fn revoke_actor(&self, actor_id: &str);
    async fn list_resources(&self, _scope: &SessionScope) -> Result<Vec<Resource>, ErrorData> {
        Ok(vec![])
    }
    async fn read_resource(
        &self,
        _uri: &str,
        _scope: &SessionScope,
    ) -> Result<ReadResourceResult, ErrorData> {
        Err(ErrorData::invalid_params("Unknown resource", None))
    }
    async fn list_prompts(&self, _scope: &SessionScope) -> Result<Vec<Prompt>, ErrorData> {
        Ok(vec![])
    }
    async fn get_prompt(
        &self,
        _name: &str,
        _arguments: JsonObject,
        _scope: &SessionScope,
    ) -> Result<GetPromptResult, ErrorData> {
        Err(ErrorData::invalid_params("Unknown prompt", None))
    }
}
