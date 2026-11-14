use super::McpConfigurationPort;
use crate::{Actor, Problem};
use agentkit_contracts::{CreateMcpServerRequest, McpServerDto, UpdateMcpServerRequest};
use agentkit_mcp::{McpConfigError, McpConfigRepository, McpConfigurationService, RepositoryFault};
use async_trait::async_trait;

fn problem<E: RepositoryFault>(error: McpConfigError<E>) -> Problem {
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

fn authorize<R: McpConfigRepository>(
    repository: &McpConfigurationService<R>,
    actor: &Actor,
) -> Result<(), Problem> {
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
impl<R: McpConfigRepository> McpConfigurationPort for McpConfigurationService<R> {
    async fn list(&self, actor: &Actor) -> Result<Vec<McpServerDto>, Problem> {
        authorize(self, actor)?;
        McpConfigurationService::list(self).await.map_err(problem)
    }
    async fn create(
        &self,
        actor: &Actor,
        input: CreateMcpServerRequest,
    ) -> Result<McpServerDto, Problem> {
        authorize(self, actor)?;
        McpConfigurationService::create(self, input)
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
        McpConfigurationService::update(self, id, input)
            .await
            .map_err(problem)
    }
    async fn delete(&self, actor: &Actor, id: &str) -> Result<(), Problem> {
        authorize(self, actor)?;
        McpConfigurationService::delete(self, id)
            .await
            .map_err(problem)
    }
}
