//! Storage read hooks exist only to run the original injected-fault tests.
use agentkit_host::http::DesktopBackend;
use agentkit_transport_http::{Actor, Backend, OperationRequest, OperationResponse, Problem};
use async_trait::async_trait;
use axum::http::HeaderMap;
use serde_json::{Value, json};
use std::sync::Arc;

pub struct FixtureBackend {
    pub inner: Arc<DesktopBackend>,
    pub read_hook: Option<(String, String)>,
}
#[async_trait]
impl Backend for FixtureBackend {
    async fn authenticate(&self, headers: &HeaderMap) -> Result<Actor, Problem> {
        self.inner.authenticate(headers).await
    }
    async fn authorize(&self, input: &OperationRequest) -> Result<(), Problem> {
        self.inner.authorize(input).await
    }
    async fn dispatch(&self, input: OperationRequest) -> Result<OperationResponse, Problem> {
        self.inner.dispatch(input).await
    }
    async fn run_status(&self, actor: &Actor, run: &str) -> Result<Option<String>, Problem> {
        self.inner.run_status(actor, run).await
    }
    async fn events(
        &self,
        actor: &Actor,
        run: &str,
        after: Option<f64>,
        limit: usize,
    ) -> Result<Vec<Value>, Problem> {
        let Some((url, token)) = &self.read_hook else {
            return self.inner.events(actor, run, after, limit).await;
        };
        if self.inner.run_status(actor, run).await?.is_none() {
            return Err(Problem::new(
                404,
                "not_found",
                format!("Run not found: {run}"),
            ));
        }
        let response = reqwest::Client::new()
            .post(url)
            .bearer_auth(token)
            .json(&json!({"runId":run,"options":{"afterSeq":after,"limit":limit}}))
            .send()
            .await
            .map_err(error)?
            .error_for_status()
            .map_err(error)?;
        response.json().await.map_err(error)
    }
}
fn error(value: impl std::fmt::Display) -> Problem {
    Problem::host("internal_error", value.to_string())
}
