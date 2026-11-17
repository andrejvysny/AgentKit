//! A [`TrustedTransport`] that answers with recorded HTTP/SSE bytes, so the
//! real Chat Completions and Responses parsers run on exactly what a server
//! would send. Requests are kept for inspection; a request beyond the queued
//! responses fails loudly instead of inventing one.
use agentkit_core::{AuthKind, ConnectionIdentity, CoreError};
use agentkit_providers::TrustedTransport;
use async_trait::async_trait;
use serde_json::Value;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Mutex;
use tokio_util::sync::CancellationToken;

/// Wire protocol a fixture is recorded for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    ChatCompletions,
    Responses,
}

impl Protocol {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ChatCompletions => "chat-completions",
            Self::Responses => "responses",
        }
    }
}

/// The bytes of `fixtures/http/<name>` in this crate.
pub fn fixture(name: &str) -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join("http")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|error| panic!("fixture {}: {error}", path.display()))
}

pub struct FixtureTransport {
    protocol: Protocol,
    responses: Mutex<VecDeque<Vec<u8>>>,
    requests: Mutex<Vec<Value>>,
}

impl FixtureTransport {
    /// One response per provider request, in order.
    pub fn new(protocol: Protocol, responses: Vec<Vec<u8>>) -> Self {
        Self {
            protocol,
            responses: Mutex::new(responses.into()),
            requests: Mutex::new(Vec::new()),
        }
    }

    /// Same, reading each response from `fixtures/http/`.
    pub fn from_fixtures(protocol: Protocol, names: &[&str]) -> Self {
        Self::new(protocol, names.iter().map(|name| fixture(name)).collect())
    }

    /// Request bodies as the provider assembled them, in send order.
    pub fn requests(&self) -> Vec<Value> {
        self.requests.lock().expect("requests lock").clone()
    }

    /// Responses nobody asked for.
    pub fn unused(&self) -> usize {
        self.responses.lock().expect("responses lock").len()
    }
}

#[async_trait]
impl TrustedTransport for FixtureTransport {
    fn identity(&self) -> ConnectionIdentity {
        ConnectionIdentity {
            provider_id: "eval-fixture".into(),
            protocol: self.protocol.as_str().into(),
            connection_id: "fixture".into(),
            generation: 1,
        }
    }

    fn auth_kind(&self) -> AuthKind {
        AuthKind::ApiKey
    }

    async fn request(
        &self,
        _: &ConnectionIdentity,
        _: &str,
        body: Option<Value>,
        _: CancellationToken,
    ) -> Result<reqwest::Response, CoreError> {
        let body = body.ok_or_else(|| CoreError::new("fixture_body", "missing request body"))?;
        self.requests.lock().expect("requests lock").push(body);
        let bytes = self
            .responses
            .lock()
            .expect("responses lock")
            .pop_front()
            .ok_or_else(|| CoreError::new("fixture_exhausted", "no recorded response left"))?;
        Ok(http::Response::builder()
            .status(200)
            .header("content-type", "text/event-stream")
            .body(bytes)
            .expect("fixture HTTP response")
            .into())
    }
}
