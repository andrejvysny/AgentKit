use super::*;
use agentkit_transport_http::{Actor, Problem};
use async_trait::async_trait;

fn problem(error: McpConfigError) -> Problem {
    let (status, code) = match error {
        McpConfigError::NotFound => (404, "not_found"),
        McpConfigError::DuplicateAlias => (409, "duplicate_alias"),
        McpConfigError::Invalid => (400, "invalid_request"),
        McpConfigError::Conflict => (409, "conflict"),
        McpConfigError::Store(_) => (500, "internal_error"),
    };
    Problem::new(status, code, "MCP configuration operation failed.")
}

impl McpConfigRepository {
    fn authorize(&self, actor: &Actor) -> Result<(), Problem> {
        if actor.id != self.app_id {
            return Err(Problem::new(
                403,
                "forbidden",
                "MCP configuration belongs to another application.",
            ));
        }
        Ok(())
    }
}

#[async_trait]
impl crate::http::McpConfigurationPort for McpConfigRepository {
    async fn list(&self, actor: &Actor) -> Result<Vec<McpServerDto>, Problem> {
        self.authorize(actor)?;
        McpConfigRepository::list(self).await.map_err(problem)
    }
    async fn create(
        &self,
        actor: &Actor,
        input: CreateMcpServerRequest,
    ) -> Result<McpServerDto, Problem> {
        self.authorize(actor)?;
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
        self.authorize(actor)?;
        McpConfigRepository::update(self, id, input)
            .await
            .map_err(problem)
    }
    async fn delete(&self, actor: &Actor, id: &str) -> Result<(), Problem> {
        self.authorize(actor)?;
        McpConfigRepository::delete(self, id).await.map_err(problem)
    }
}
