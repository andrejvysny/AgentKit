//! The live model behind the production provider adapters. Only the HTTP
//! transport is new: request bodies, SSE parsing, profile sampling and tool
//! dialect all come from `ChatCompletionsProvider` / `ResponsesProvider`.
//! The key lives in the transport and is never printed or put in an error.
use agentkit_core::{AuthKind, ChatProvider, ConnectionIdentity, CoreError};
use agentkit_providers::{ChatCompletionsProvider, ResponsesProvider, TrustedTransport};
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub struct LiveEnv {
    pub base_url: String,
    pub model: String,
    pub key: String,
    /// Provider kind the profile is matched on (`openai-compatible`, `openai`, `anthropic`).
    pub provider: String,
    /// The server behind the endpoint (`litellm`, `vllm`, ...); recorded only.
    pub server: String,
    /// `chat-completions` or `responses`.
    pub protocol: String,
}

impl LiveEnv {
    /// `None` unless `AGENTKIT_LIVE_CONFORMANCE=1`; the endpoint settings are required then.
    pub fn from_env() -> Result<Option<Self>, String> {
        if std::env::var("AGENTKIT_LIVE_CONFORMANCE").as_deref() != Ok("1") {
            return Ok(None);
        }
        let need = |name: &str| {
            std::env::var(name)
                .map_err(|_| format!("{name} is required with AGENTKIT_LIVE_CONFORMANCE=1"))
        };
        let get =
            |name: &str, default: &str| std::env::var(name).unwrap_or_else(|_| default.to_owned());
        let protocol = get("AGENTKIT_LIVE_PROTOCOL", "chat-completions");
        if protocol != "chat-completions" && protocol != "responses" {
            return Err("AGENTKIT_LIVE_PROTOCOL must be chat-completions or responses".into());
        }
        Ok(Some(Self {
            base_url: need("AGENTKIT_LIVE_BASE_URL")?,
            model: need("AGENTKIT_LIVE_MODEL")?,
            key: get("AGENTKIT_LIVE_API_KEY", ""),
            provider: get("AGENTKIT_LIVE_PROVIDER", "openai-compatible"),
            server: get("AGENTKIT_LIVE_SERVER", "unspecified"),
            protocol,
        }))
    }

    pub fn provider(&self) -> Result<Arc<dyn ChatProvider>, CoreError> {
        let transport = Arc::new(LiveTransport {
            base_url: self.base_url.trim_end_matches('/').to_owned(),
            key: self.key.clone(),
            protocol: self.protocol.clone(),
            client: reqwest::Client::new(),
        });
        Ok(match self.protocol.as_str() {
            "responses" => Arc::new(ResponsesProvider::new(transport)?),
            _ => Arc::new(ChatCompletionsProvider::new(transport)?),
        })
    }
}

struct LiveTransport {
    base_url: String,
    key: String,
    protocol: String,
    client: reqwest::Client,
}

#[async_trait]
impl TrustedTransport for LiveTransport {
    fn identity(&self) -> ConnectionIdentity {
        ConnectionIdentity {
            provider_id: "live".into(),
            protocol: self.protocol.clone(),
            connection_id: "live".into(),
            generation: 1,
        }
    }

    fn auth_kind(&self) -> AuthKind {
        AuthKind::ApiKey
    }

    async fn request(
        &self,
        _identity: &ConnectionIdentity,
        operation: &str,
        body: Option<Value>,
        cancellation: CancellationToken,
    ) -> Result<reqwest::Response, CoreError> {
        let mut request = self
            .client
            .post(format!("{}/{operation}", self.base_url))
            .header("content-type", "application/json")
            .body(body.unwrap_or(Value::Null).to_string());
        if !self.key.is_empty() {
            request = request.bearer_auth(&self.key);
        }
        tokio::select! { biased;
            () = cancellation.cancelled() => Err(CoreError::new("cancelled", "aborted")),
            sent = request.send() => sent.map_err(|_| CoreError::new("transport_error", "live request failed")),
        }
    }
}
