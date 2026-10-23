use crate::json_fields::JsonFields;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{
    ClientHandler, RoleClient, ServiceExt,
    model::*,
    service::{Peer, RunningService},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};
use tokio::{process::Command, sync::Mutex};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct ClientOptions {
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
    pub max_tool_pages: usize,
    pub max_tools: usize,
}
impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(5),
            max_tool_pages: 100,
            max_tools: 10_000,
        }
    }
}

#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
pub enum McpClientError {
    #[error("MCP connection failed")]
    Connect,
    #[error("MCP request failed; delivery may be ambiguous")]
    Request,
    #[error("MCP deadline exceeded; delivery may be ambiguous")]
    Timeout,
    #[error("MCP operation cancelled; delivery may be ambiguous")]
    Cancelled,
    #[error("MCP client closed")]
    Closed,
    #[error("MCP pagination bound or repeated cursor")]
    Pagination,
    #[error("MCP negotiated unsupported protocol revision")]
    Protocol,
    #[error("MCP endpoint or transport configuration invalid")]
    Configuration,
    #[error("MCP transport disconnected; delivery may be ambiguous")]
    Disconnected,
    #[error("MCP connect circuit is open")]
    CircuitOpen,
    #[error("MCP secret reference is unavailable")]
    SecretMissing,
    #[error("MCP reconnect attempts exhausted")]
    ReconnectExhausted,
    #[error("MCP dispatch authority revoked")]
    Authority,
}

impl McpClientError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Connect => "mcp_connect_failed",
            Self::Timeout => "mcp_request_timeout",
            Self::Cancelled => "mcp_request_aborted",
            Self::Closed | Self::Disconnected => "mcp_not_connected",
            Self::Configuration => "mcp_invalid_config",
            Self::CircuitOpen => "mcp_circuit_open",
            Self::SecretMissing => "mcp_secret_missing",
            Self::ReconnectExhausted => "mcp_reconnect_exhausted",
            Self::Authority => "mcp_authority_revoked",
            _ => "mcp_remote_error",
        }
    }
}

pub(crate) struct FrozenClient;
impl ClientHandler for FrozenClient {
    fn get_info(&self) -> ClientConfig {
        ClientConfig::new(
            ClientCapabilities::default(),
            Implementation::new("agentkit-mcp-client", "0.1.0"),
        )
        .with_protocol_version(ProtocolVersion::V_2025_11_25)
    }
}

pub struct McpClient {
    service: Mutex<Option<RunningService<RoleClient, FrozenClient>>>,
    options: ClientOptions,
}

impl McpClient {
    pub async fn stdio(
        executable: &str,
        arguments: &[String],
        environment: &BTreeMap<String, String>,
        options: ClientOptions,
    ) -> Result<Self, McpClientError> {
        let mut command = Command::new(executable);
        command
            .args(arguments)
            .env_clear()
            .envs(environment)
            .kill_on_drop(true);
        let transport = crate::stdio_client::JsonChild::spawn(&mut command)
            .map_err(|_| McpClientError::Connect)?;
        let service = tokio::time::timeout(options.connect_timeout, FrozenClient.serve(transport))
            .await
            .map_err(|_| McpClientError::Timeout)?
            .map_err(|_| McpClientError::Connect)?;
        Self::connected(service, options).await
    }

    pub async fn http(
        endpoint: &str,
        bearer_token: Option<&str>,
        options: ClientOptions,
    ) -> Result<Self, McpClientError> {
        Self::http_with_headers(endpoint, bearer_token, http::HeaderMap::new(), options).await
    }

    /// Headers contain resolved secrets only in memory; errors never quote them.
    pub async fn http_with_headers(
        endpoint: &str,
        bearer_token: Option<&str>,
        headers: http::HeaderMap,
        options: ClientOptions,
    ) -> Result<Self, McpClientError> {
        validate_endpoint(endpoint)?;
        if bearer_token.is_some() && headers.contains_key(http::header::AUTHORIZATION)
            || [
                "host",
                "mcp-session-id",
                "last-event-id",
                "mcp-protocol-version",
                "content-type",
                "accept",
            ]
            .iter()
            .any(|name| headers.contains_key(*name))
        {
            return Err(McpClientError::Configuration);
        }
        let client = reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(options.connect_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| McpClientError::Connect)?;
        let mut config = StreamableHttpClientTransportConfig::with_uri(endpoint.to_owned())
            .reinit_on_expired_session(false)
            .max_sse_event_size(1_048_576)
            .custom_headers(
                headers
                    .iter()
                    .map(|(name, value)| (name.clone(), value.clone()))
                    .collect(),
            );
        if let Some(token) = bearer_token {
            config = config.auth_header(token);
        }
        let transport = StreamableHttpClientTransport::with_client(
            crate::http_client::JsonHttpClient(client),
            config,
        );
        let service = tokio::time::timeout(options.connect_timeout, FrozenClient.serve(transport))
            .await
            .map_err(|_| McpClientError::Timeout)?
            .map_err(|_| McpClientError::Connect)?;
        Self::connected(service, options).await
    }

    async fn connected(
        mut service: RunningService<RoleClient, FrozenClient>,
        options: ClientOptions,
    ) -> Result<Self, McpClientError> {
        let valid = service.peer_info().is_some_and(|info| {
            crate::TS_PROTOCOL_VERSIONS.contains(&info.protocol_version.as_str())
        });
        if !valid {
            let _ = service.close_with_timeout(Duration::from_secs(5)).await;
            return Err(McpClientError::Protocol);
        }
        Ok(Self {
            service: Mutex::new(Some(service)),
            options,
        })
    }

    async fn peer(&self) -> Result<Peer<RoleClient>, McpClientError> {
        self.service
            .lock()
            .await
            .as_ref()
            .filter(|service| !service.is_closed())
            .map(|service| service.peer().clone())
            .ok_or(McpClientError::Closed)
    }

    pub async fn list_tools(
        &self,
        cancellation: CancellationToken,
    ) -> Result<Vec<Tool>, McpClientError> {
        let peer = self.peer().await?;
        let mut cursor = None;
        let mut seen = BTreeSet::new();
        let mut tools = Vec::new();
        for _ in 0..self.options.max_tool_pages {
            let request =
                peer.list_tools(Some(PaginatedRequestParams::default().with_cursor(cursor)));
            let mut page = self.bounded(request, &cancellation).await?;
            page.map_json(crate::json_boundary::restore);
            tools.extend(page.tools);
            if tools.len() > self.options.max_tools {
                return Err(McpClientError::Pagination);
            }
            cursor = page.next_cursor;
            let Some(next) = cursor.as_ref() else {
                return Ok(tools);
            };
            if !seen.insert(next.clone()) {
                return Err(McpClientError::Pagination);
            }
        }
        Err(McpClientError::Pagination)
    }

    /// Never retries a call: timeout, disconnect and cancellation may hide effects.
    pub async fn call_tool(
        &self,
        name: &str,
        arguments: JsonObject,
        cancellation: CancellationToken,
    ) -> Result<CallToolResult, McpClientError> {
        self.call_tool_guarded(name, arguments, cancellation, None)
            .await
    }

    pub async fn call_tool_guarded(
        &self,
        name: &str,
        arguments: JsonObject,
        cancellation: CancellationToken,
        guard: Option<std::sync::Arc<dyn agentkit_core::DispatchGuard>>,
    ) -> Result<CallToolResult, McpClientError> {
        let peer = self.peer().await?;
        let arguments =
            agentkit_contracts::javascript_json_value(&serde_json::Value::Object(arguments))
                .as_object()
                .cloned()
                .expect("MCP arguments object");
        crate::manager::check_dispatch(guard, &cancellation).await?;
        let mut result = self
            .bounded(
                peer.call_tool(
                    CallToolRequestParams::new(name.to_owned()).with_arguments(arguments),
                ),
                &cancellation,
            )
            .await?;
        result.map_json(crate::json_boundary::restore);
        Ok(result)
    }

    async fn bounded<T>(
        &self,
        request: impl Future<Output = Result<T, rmcp::service::ServiceError>>,
        cancellation: &CancellationToken,
    ) -> Result<T, McpClientError> {
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(McpClientError::Cancelled),
            result = tokio::time::timeout(self.options.request_timeout, request) =>
                result.map_err(|_| McpClientError::Timeout)?.map_err(classify_request),
        }
    }

    pub async fn close(&self) -> Result<(), McpClientError> {
        let service = self.service.lock().await.take();
        if let Some(mut service) = service {
            let closed = service
                .close_with_timeout(Duration::from_secs(5))
                .await
                .map_err(|_| McpClientError::Request)?;
            if closed.is_none() {
                return Err(McpClientError::Timeout);
            }
        }
        Ok(())
    }
}

fn classify_request(error: rmcp::service::ServiceError) -> McpClientError {
    use rmcp::service::ServiceError;
    match error {
        ServiceError::TransportClosed | ServiceError::TransportSend(_) => {
            McpClientError::Disconnected
        }
        ServiceError::Timeout { .. } => McpClientError::Timeout,
        ServiceError::Cancelled { .. } => McpClientError::Cancelled,
        _ => McpClientError::Request,
    }
}

fn validate_endpoint(endpoint: &str) -> Result<(), McpClientError> {
    let url = url::Url::parse(endpoint).map_err(|_| McpClientError::Configuration)?;
    let local = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    if url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !(url.scheme() == "https" || url.scheme() == "http" && local)
    {
        return Err(McpClientError::Configuration);
    }
    Ok(())
}
