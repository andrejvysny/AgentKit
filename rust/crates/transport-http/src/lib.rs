//! Optional REST/SSE adapter. Backends own durable state and actor isolation.
mod handler;
mod idempotency;
mod problem;
mod routes;
mod stream;
mod validation;

pub use handler::{Config, router};
pub use idempotency::{Idempotency, derive_task_id};
pub use problem::Problem;
pub use routes::{Route, routes};
pub use stream::StreamOptions;

use async_trait::async_trait;
use axum::http::HeaderMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Identity established by the backend; never inferred from request JSON.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Actor {
    pub id: String,
    #[serde(
        default,
        deserialize_with = "agentkit_contracts::deserialize_preserving"
    )]
    pub metadata: BTreeMap<String, Value>,
}

#[derive(Debug, Clone)]
pub struct OperationRequest {
    pub actor: Actor,
    pub operation: String,
    pub params: BTreeMap<String, String>,
    pub query: BTreeMap<String, String>,
    pub body: Value,
    pub idempotency: Option<Idempotency>,
}

#[derive(Debug, Clone)]
pub struct OperationResponse {
    pub status: u16,
    pub body: Value,
}

impl OperationResponse {
    pub fn json(status: u16, body: Value) -> Self {
        Self { status, body }
    }
}

/// Every call must enforce actor ownership, including indirect resource lookups.
/// `dispatch` must atomically check/store idempotency fingerprints and results;
/// repeated keys with changed bodies return `idempotency_key_mismatch` (422).
/// Event pages contain only committed events, sorted by strictly increasing numeric sequence.
/// Terminal task status is published only after all final events are committed.
#[async_trait]
pub trait Backend: Send + Sync + 'static {
    async fn authenticate(&self, headers: &HeaderMap) -> Result<Actor, Problem>;
    async fn authorize(&self, request: &OperationRequest) -> Result<(), Problem>;
    async fn dispatch(&self, request: OperationRequest) -> Result<OperationResponse, Problem>;
    async fn events(
        &self,
        actor: &Actor,
        run_id: &str,
        after_seq: Option<f64>,
        limit: usize,
    ) -> Result<Vec<Value>, Problem>;
    async fn run_status(&self, actor: &Actor, run_id: &str) -> Result<Option<String>, Problem>;
}
