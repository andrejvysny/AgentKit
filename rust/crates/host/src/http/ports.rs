use agentkit_contracts::{
    AiProviderModel, AiToolDefinition, CreateMcpServerRequest, GrantAllowanceRequest, McpServerDto,
    TestProviderResponse, UpdateMcpServerRequest, WriteAllowanceDto,
};
use agentkit_transport_http::{Actor, Problem};
use async_trait::async_trait;

/// The host owns policy enforcement; the adapter never grants writes by itself.
#[async_trait]
pub trait WritePolicyPort: Send + Sync {
    async fn list(&self, actor: &Actor, chat_id: &str) -> Result<Vec<WriteAllowanceDto>, Problem>;
    async fn grant(
        &self,
        actor: &Actor,
        chat_id: &str,
        input: GrantAllowanceRequest,
    ) -> Result<WriteAllowanceDto, Problem>;
    async fn revoke(&self, actor: &Actor, chat_id: &str, key: &str) -> Result<(), Problem>;
}

#[async_trait]
pub trait McpConfigurationPort: Send + Sync {
    async fn list(&self, actor: &Actor) -> Result<Vec<McpServerDto>, Problem>;
    async fn create(
        &self,
        actor: &Actor,
        input: CreateMcpServerRequest,
    ) -> Result<McpServerDto, Problem>;
    async fn update(
        &self,
        actor: &Actor,
        id: &str,
        input: UpdateMcpServerRequest,
    ) -> Result<McpServerDto, Problem>;
    async fn delete(&self, actor: &Actor, id: &str) -> Result<(), Problem>;
}

#[async_trait]
pub trait ToolCatalogPort: Send + Sync {
    async fn list(&self, actor: &Actor) -> Result<Vec<AiToolDefinition>, Problem>;
}

/// Implementations must obtain explicit native connection consent before network I/O.
#[async_trait]
pub trait ProviderOperations: Send + Sync {
    async fn refresh_models(
        &self,
        actor: &Actor,
        provider_id: &str,
    ) -> Result<Vec<AiProviderModel>, Problem>;
    async fn test(&self, actor: &Actor, provider_id: &str)
    -> Result<TestProviderResponse, Problem>;
}
