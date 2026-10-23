use crate::{
    Capability, ConnectionError, ConnectionNetwork, NetworkResponse, RequestPlan, ResolvedSecrets,
    Secret,
};
use async_trait::async_trait;
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

/// OpenAI-compatible HTTP transport. Routes are explicit in the approved plan.
pub struct ReqwestConnectionNetwork {
    client: reqwest::Client,
    max_body_bytes: usize,
}

impl ReqwestConnectionNetwork {
    pub fn new(timeout: Duration, max_body_bytes: usize) -> Result<Self, ConnectionError> {
        if timeout.is_zero() || max_body_bytes == 0 {
            return Err(ConnectionError::InvalidConfiguration);
        }
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(timeout)
            .build()
            .map_err(|_| ConnectionError::NetworkUnavailable)?;
        Ok(Self {
            client,
            max_body_bytes,
        })
    }

    /// Use through `NativeConnections::execute_with` and fence each stream event.
    /// The response body is deliberately left unread for the streaming consumer.
    pub async fn send_streaming(
        &self,
        plan: &RequestPlan,
        credential: Option<&Secret>,
    ) -> Result<reqwest::Response, ConnectionError> {
        if !plan.connection().config.extra_header_names.is_empty() {
            return Err(ConnectionError::NetworkUnavailable);
        }
        let secrets = ResolvedSecrets {
            api_key: credential.map(|secret| Secret::new(secret.expose())),
            headers: BTreeMap::new(),
        };
        self.send_streaming_resolved(plan, &secrets).await
    }

    pub async fn send_streaming_resolved(
        &self,
        plan: &RequestPlan,
        secrets: &ResolvedSecrets,
    ) -> Result<reqwest::Response, ConnectionError> {
        self.check_available(plan)?;
        let headers = request_headers(plan, secrets)?;
        let read_only = matches!(
            plan.capability(),
            Capability::ModelDiscovery | Capability::Probe
        );
        let request = if read_only {
            self.client.get(plan.destination().clone())
        } else {
            self.client
                .post(plan.destination().clone())
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(plan.payload().to_vec())
        };
        let response = request
            .headers(headers)
            .send()
            .await
            .map_err(|_| ConnectionError::NetworkFailed)?;
        if response.status().is_redirection() {
            return Err(ConnectionError::NetworkFailed);
        }
        Ok(response)
    }

    async fn read_response(
        &self,
        mut response: reqwest::Response,
    ) -> Result<NetworkResponse, ConnectionError> {
        if response
            .content_length()
            .is_some_and(|length| length > self.max_body_bytes as u64)
        {
            return Err(ConnectionError::NetworkFailed);
        }
        let status = response.status().as_u16();
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| ConnectionError::NetworkFailed)?
        {
            if chunk.len() > self.max_body_bytes.saturating_sub(body.len()) {
                return Err(ConnectionError::NetworkFailed);
            }
            body.extend_from_slice(&chunk);
        }
        Ok(NetworkResponse { status, body })
    }
}

fn request_headers(
    plan: &RequestPlan,
    secrets: &ResolvedSecrets,
) -> Result<reqwest::header::HeaderMap, ConnectionError> {
    let names: BTreeSet<_> = secrets.headers.keys().cloned().collect();
    if names != plan.connection().config.extra_header_names
        || secrets.api_key.is_some() != plan.connection().config.credential_required
    {
        return Err(ConnectionError::InvalidConfiguration);
    }
    let mut headers = reqwest::header::HeaderMap::new();
    for (name, secret) in &secrets.headers {
        let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| ConnectionError::InvalidConfiguration)?;
        let mut value = reqwest::header::HeaderValue::from_str(secret.expose())
            .map_err(|_| ConnectionError::InvalidConfiguration)?;
        value.set_sensitive(true);
        headers.insert(name, value);
    }
    if let Some(secret) = &secrets.api_key {
        let mut value =
            reqwest::header::HeaderValue::from_str(&format!("Bearer {}", secret.expose()))
                .map_err(|_| ConnectionError::InvalidConfiguration)?;
        value.set_sensitive(true);
        headers.insert(reqwest::header::AUTHORIZATION, value);
    }
    Ok(headers)
}

#[async_trait]
impl ConnectionNetwork for ReqwestConnectionNetwork {
    fn supports_extra_headers(&self) -> bool {
        true
    }
    fn check_available(&self, plan: &RequestPlan) -> Result<(), ConnectionError> {
        let kind = plan.connection().config.provider_kind.as_str();
        if !matches!(
            kind,
            "openai" | "openrouter" | "openai-compatible" | "lmstudio" | "omlx" | "ollama"
        ) {
            return Err(ConnectionError::NetworkUnavailable);
        }
        if matches!(
            plan.capability(),
            Capability::ModelDiscovery | Capability::Probe
        ) {
            if !plan.payload().is_empty() {
                return Err(ConnectionError::InvalidConfiguration);
            }
        } else {
            let text = std::str::from_utf8(plan.payload())
                .map_err(|_| ConnectionError::InvalidConfiguration)?;
            let body = agentkit_contracts::parse_javascript_json(text)
                .map_err(|_| ConnectionError::InvalidConfiguration)?;
            if body.get("model").and_then(serde_json::Value::as_str) != plan.model() {
                return Err(ConnectionError::UnsupportedModel);
            }
        }
        Ok(())
    }

    async fn send(
        &self,
        plan: &RequestPlan,
        credential: Option<&Secret>,
    ) -> Result<NetworkResponse, ConnectionError> {
        self.read_response(self.send_streaming(plan, credential).await?)
            .await
    }

    async fn send_resolved(
        &self,
        plan: &RequestPlan,
        secrets: &ResolvedSecrets,
    ) -> Result<NetworkResponse, ConnectionError> {
        self.read_response(self.send_streaming_resolved(plan, secrets).await?)
            .await
    }
}
