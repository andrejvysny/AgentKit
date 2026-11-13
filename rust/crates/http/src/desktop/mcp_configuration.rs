use super::McpConfigurationPort;
use crate::{Actor, Problem};
use agentkit_contracts::{CreateMcpServerRequest, McpServerDto, UpdateMcpServerRequest};
use agentkit_mcp::{McpConfigError, McpConfigRepository};
use async_trait::async_trait;

fn problem(error: McpConfigError) -> Problem {
    let (status, code) = match error {
        McpConfigError::NotFound => (404, "not_found"),
        McpConfigError::DuplicateAlias => (409, "duplicate_alias"),
        McpConfigError::Invalid => (400, "invalid_request"),
        McpConfigError::Conflict => (409, "conflict"),
        McpConfigError::Store(_) => (500, "internal_error"),
        McpConfigError::SecretStore => (501, "secret_store_unavailable"),
        McpConfigError::Runtime => (503, "mcp_runtime_unavailable"),
    };
    Problem::new(status, code, "MCP configuration operation failed.")
}

fn authorize(repository: &McpConfigRepository, actor: &Actor) -> Result<(), Problem> {
    if actor.id != repository.app_id() {
        return Err(Problem::new(
            403,
            "forbidden",
            "MCP configuration belongs to another application.",
        ));
    }
    Ok(())
}

#[async_trait]
impl McpConfigurationPort for McpConfigRepository {
    async fn list(&self, actor: &Actor) -> Result<Vec<McpServerDto>, Problem> {
        authorize(self, actor)?;
        McpConfigRepository::list(self).await.map_err(problem)
    }
    async fn create(
        &self,
        actor: &Actor,
        input: CreateMcpServerRequest,
    ) -> Result<McpServerDto, Problem> {
        authorize(self, actor)?;
        McpConfigRepository::create(self, input)
            .await
            .map_err(problem)
    }
    async fn update(
        &self,
        actor: &Actor,
        id: &str,
        input: UpdateMcpServerRequest,
    ) -> Result<McpServerDto, Problem> {
        authorize(self, actor)?;
        McpConfigRepository::update(self, id, input)
            .await
            .map_err(problem)
    }
    async fn delete(&self, actor: &Actor, id: &str) -> Result<(), Problem> {
        authorize(self, actor)?;
        McpConfigRepository::delete(self, id).await.map_err(problem)
    }
}
