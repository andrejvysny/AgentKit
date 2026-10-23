use crate::{ClientError, error::checked};
use agentkit_contracts::RestOperation;
use futures::StreamExt;
use reqwest::{Client as HttpClient, Url, header::HeaderMap};
use serde_json::Value;
use std::{collections::BTreeMap, time::Duration};

#[derive(Debug, Clone, Default)]
pub struct Call {
    pub path: BTreeMap<String, String>,
    pub query: BTreeMap<String, String>,
    pub body: Option<Value>,
    pub headers: HeaderMap,
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Client {
    pub(crate) base_url: String,
    pub(crate) http: HttpClient,
    pub(crate) headers: HeaderMap,
    pub max_response_bytes: usize,
    pub max_idempotent_retries: usize,
    pub retry_delay: Duration,
}

impl Client {
    pub fn new(base_url: &str) -> Result<Self, ClientError> {
        let url = Url::parse(base_url).map_err(|e| ClientError::InvalidRequest(e.to_string()))?;
        if !matches!(url.scheme(), "http" | "https")
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(ClientError::InvalidRequest(
                "Base URL must be HTTP(S), without query or fragment".into(),
            ));
        }
        Ok(Self {
            base_url: base_url.trim_end_matches('/').into(),
            http: HttpClient::new(),
            headers: HeaderMap::new(),
            max_response_bytes: 4 * 1_048_576,
            max_idempotent_retries: 2,
            retry_delay: Duration::from_millis(100),
        })
    }

    pub fn with_headers(mut self, headers: HeaderMap) -> Self {
        self.headers = headers;
        self
    }
    pub fn with_http_client(mut self, client: HttpClient) -> Self {
        self.http = client;
        self
    }

    /// Exposes the complete operation table without transcribing endpoint paths.
    pub async fn call(&self, operation: RestOperation, call: Call) -> Result<Value, ClientError> {
        let url = self.url(&operation, &call)?;
        self.validate(&operation, &call)?;
        let retry_key = if matches!(
            operation,
            RestOperation::SubmitMessage | RestOperation::RegenerateMessage
        ) {
            call.idempotency_key.as_ref()
        } else {
            None
        };
        let mut attempts = 0;
        loop {
            let response = match self.send(&operation, &call, url.clone()).await {
                Ok(response) => checked(response).await?,
                Err(error) => {
                    if let Some(key) = retry_key {
                        if attempts < self.max_idempotent_retries {
                            attempts += 1;
                            tokio::time::sleep(self.retry_delay).await;
                            continue;
                        }
                        return Err(ClientError::UncertainDelivery {
                            key: key.clone(),
                            cause: error.to_string(),
                        });
                    }
                    return Err(error.into());
                }
            };
            match self.read(response).await {
                Ok(value) => return Ok(value),
                Err(ClientError::Transport(error)) if retry_key.is_some() => {
                    if attempts < self.max_idempotent_retries {
                        attempts += 1;
                        tokio::time::sleep(self.retry_delay).await;
                        continue;
                    }
                    return Err(ClientError::UncertainDelivery {
                        key: retry_key.unwrap().clone(),
                        cause: error.to_string(),
                    });
                }
                Err(error) => return Err(error),
            }
        }
    }

    pub async fn call_typed<T: agentkit_contracts::JsonDeserialize>(
        &self,
        operation: RestOperation,
        call: Call,
    ) -> Result<T, ClientError> {
        Ok(T::from_json(self.call(operation, call).await?)?)
    }

    pub async fn submit_message(
        &self,
        chat_id: &str,
        body: Value,
        key: &str,
    ) -> Result<Value, ClientError> {
        self.call(
            RestOperation::SubmitMessage,
            Call {
                path: BTreeMap::from([("chatId".into(), chat_id.into())]),
                body: Some(body),
                idempotency_key: Some(key.into()),
                ..Call::default()
            },
        )
        .await
    }

    pub(crate) async fn send(
        &self,
        operation: &RestOperation,
        call: &Call,
        url: Url,
    ) -> Result<reqwest::Response, reqwest::Error> {
        let route = operation.route();
        let method = reqwest::Method::from_bytes(route.method.as_str().as_bytes()).unwrap();
        let mut request = self
            .http
            .request(method, url)
            .headers(self.headers.clone())
            .headers(call.headers.clone());
        if let Some(body) = &call.body {
            request = request.json(&agentkit_contracts::javascript_json_value(body));
        }
        if let Some(key) = &call.idempotency_key {
            request = request.header("idempotency-key", key);
        }
        request.send().await
    }

    pub(crate) fn url(&self, operation: &RestOperation, call: &Call) -> Result<Url, ClientError> {
        let mut url =
            Url::parse(&self.base_url).map_err(|e| ClientError::InvalidRequest(e.to_string()))?;
        let mut path = url
            .path_segments_mut()
            .map_err(|_| ClientError::InvalidRequest("Base URL cannot be a base".into()))?;
        path.pop_if_empty();
        for segment in operation.route().path.split('/').filter(|s| !s.is_empty()) {
            let value = if let Some(name) = segment.strip_prefix(':') {
                call.path.get(name).ok_or_else(|| {
                    ClientError::InvalidRequest(format!("Missing path parameter `{name}`"))
                })?
            } else {
                segment
            };
            path.push(value);
        }
        drop(path);
        url.query_pairs_mut().extend_pairs(&call.query);
        Ok(url)
    }

    fn validate(&self, operation: &RestOperation, call: &Call) -> Result<(), ClientError> {
        if matches!(
            operation,
            RestOperation::SubmitMessage | RestOperation::RegenerateMessage
        ) && call
            .idempotency_key
            .as_ref()
            .is_none_or(|s| s.trim().is_empty())
        {
            return Err(ClientError::InvalidRequest(
                "Idempotency-Key is required".into(),
            ));
        }
        let empty = if operation.request_schema().is_some() {
            serde_json::json!({})
        } else {
            Value::Null
        };
        agentkit_contracts::validate_request(*operation, call.body.as_ref().unwrap_or(&empty))
            .map_err(|e| ClientError::InvalidRequest(e.to_string()))
    }

    async fn read(&self, response: reqwest::Response) -> Result<Value, ClientError> {
        let mut bytes = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            if bytes.len().saturating_add(chunk.len()) > self.max_response_bytes {
                return Err(ClientError::Protocol(
                    "HTTP response exceeds max_response_bytes".into(),
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        if bytes.is_empty() {
            Ok(Value::Null)
        } else {
            Ok(agentkit_contracts::parse_javascript_json(
                &String::from_utf8_lossy(&bytes),
            )?)
        }
    }
}
