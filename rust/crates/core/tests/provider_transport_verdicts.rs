//! Transport verdicts keep their classification. A connection revoked
//! between turns makes the transport answer `connection_changed` BEFORE any
//! HTTP — collapsing that into `network_error` would misreport an operator
//! revocation as an outage (and make it look retryable). Only genuine
//! transport failures (the `transport_error` the transports raise for a
//! refused/DNS/TLS failure) are classified `network_error`.

use agentkit_core::{providers::*, *};
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::Value;
use std::sync::Arc;

struct VerdictTransport {
    code: String,
    message: String,
}

#[async_trait]
impl TrustedTransport for VerdictTransport {
    fn identity(&self) -> ConnectionIdentity {
        ConnectionIdentity {
            provider_id: "test".into(),
            protocol: "chat-completions".into(),
            connection_id: "c".into(),
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
        _: Option<Value>,
        _: CancellationToken,
    ) -> Result<reqwest::Response, CoreError> {
        Err(CoreError::new(&self.code, &self.message))
    }
}

fn provider(code: &str, message: &str) -> ChatCompletionsProvider {
    ChatCompletionsProvider::new(Arc::new(VerdictTransport {
        code: code.to_owned(),
        message: message.to_owned(),
    }))
    .unwrap()
}

fn request() -> ChatRequest {
    ChatRequest {
        model: "m".into(),
        messages: vec![ChatMessage::text("user", "hi")],
        ..Default::default()
    }
}

async fn failure(code: &str, message: &str) -> CoreError {
    let events: Vec<_> = provider(code, message)
        .stream_chat(request(), CancellationToken::new())
        .collect()
        .await;
    events
        .into_iter()
        .find_map(Result::err)
        .expect("the stream must fail")
}

#[tokio::test]
async fn revoked_connection_verdict_survives_the_chat_lane_classification() {
    let error = failure("connection_changed", "Provider connection revoked").await;
    assert_eq!(error.code, "connection_changed");
    assert!(error.message.contains("revoked"));
}

#[tokio::test]
async fn genuine_transport_failure_is_still_classified_network_error() {
    let error = failure("transport_error", "ECONNREFUSED 127.0.0.1:1").await;
    assert_eq!(error.code, "network_error");
    assert!(error.message.contains("ECONNREFUSED"));
}
