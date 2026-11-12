use super::*;
use crate::jsbridge::{JsBridge, PendingKind};
use agentkit_mcp::{ManagedOAuthOptions, ManagedOAuthProvider, OAuthBrowser, OAuthSecrets};
use napi::threadsafe_function::ThreadsafeFunction;
use rmcp_auth::{OAuthHttpClient, OAuthHttpClientFuture, OAuthHttpRequest};

// Access through the direct dependency's public module, without sharing raw
// token material with the host's durable MCP configuration repository.
use agentkit_mcp::oauth_protocol as rmcp_auth;

struct SecretPort {
    bridge: Arc<JsBridge>,
}
struct BrowserPort {
    bridge: Arc<JsBridge>,
    alias: String,
}
#[derive(Clone)]
struct HttpPort {
    bridge: Arc<JsBridge>,
}

pub(super) async fn callback(
    bridge: &Arc<JsBridge>,
    kind: PendingKind,
    request: Value,
) -> std::result::Result<Value, McpClientError> {
    let id = uuid::Uuid::new_v4().to_string();
    bridge
        .clone()
        .request(kind, &id, &request, &CancellationToken::new())
        .await
        .map_err(|_| McpClientError::AuthRequired)
}

#[async_trait]
impl OAuthSecrets for SecretPort {
    async fn get(&self, key: &str) -> std::result::Result<Option<String>, McpClientError> {
        let value = callback(
            &self.bridge,
            PendingKind::McpOAuthSecret,
            json!({"operation":"get","key":key}),
        )
        .await?;
        match value {
            Value::Null => Ok(None),
            Value::String(value) => Ok(Some(value)),
            _ => Err(McpClientError::AuthRequired),
        }
    }
    async fn set(&self, key: &str, value: &str) -> std::result::Result<(), McpClientError> {
        callback(
            &self.bridge,
            PendingKind::McpOAuthSecret,
            json!({"operation":"set","key":key,"value":value}),
        )
        .await
        .map(|_| ())
    }
    async fn delete(&self, key: &str) -> std::result::Result<(), McpClientError> {
        callback(
            &self.bridge,
            PendingKind::McpOAuthSecret,
            json!({"operation":"delete","key":key}),
        )
        .await
        .map(|_| ())
    }
}
#[async_trait]
impl OAuthBrowser for BrowserPort {
    async fn redirect(&self, url: &str) -> std::result::Result<(), McpClientError> {
        callback(
            &self.bridge,
            PendingKind::McpOAuthRedirect,
            json!({"alias":self.alias,"url":url}),
        )
        .await
        .map(|_| ())
    }
}
impl OAuthHttpClient for HttpPort {
    fn execute(&self, request: OAuthHttpRequest) -> OAuthHttpClientFuture<'_> {
        Box::pin(async move {
            let request = request.request;
            let headers = request
                .headers()
                .iter()
                .map(|(key, value)| {
                    value
                        .to_str()
                        .map(|value| (key.to_string(), value.to_owned()))
                        .map_err(|_| McpClientError::Configuration)
                })
                .collect::<std::result::Result<BTreeMap<_, _>, _>>()?;
            let body =
                std::str::from_utf8(request.body()).map_err(|_| McpClientError::Configuration)?;
            let value=callback(&self.bridge,PendingKind::McpOAuthHttp,
                json!({"url":request.uri().to_string(),"method":request.method().as_str(),"headers":headers,"body":body,"redirect":"error"})).await?;
            let status = value["status"]
                .as_u64()
                .filter(|status| (100..=599).contains(status))
                .ok_or(McpClientError::Configuration)?;
            let body = value["body"]
                .as_str()
                .ok_or(McpClientError::Configuration)?;
            if body.len() > 4_194_304 {
                return Err(McpClientError::Configuration.into());
            }
            let mut response = http::Response::builder().status(status as u16);
            if let Some(headers) = value.get("headers") {
                for (name, value) in headers.as_object().ok_or(McpClientError::Configuration)? {
                    response =
                        response.header(name, value.as_str().ok_or(McpClientError::Configuration)?);
                }
            }
            response
                .body(body.as_bytes().to_vec())
                .map_err(|_| McpClientError::Configuration.into())
        })
    }
}

#[async_trait]
impl agentkit_mcp::McpHttpExecutor for HttpPort {
    async fn execute(
        &self,
        request: reqwest::Request,
    ) -> std::result::Result<reqwest::Response, McpClientError> {
        let headers = request
            .headers()
            .iter()
            .map(|(name, value)| {
                value
                    .to_str()
                    .map(|value| (name.to_string(), value.to_owned()))
                    .map_err(|_| McpClientError::Configuration)
            })
            .collect::<std::result::Result<BTreeMap<_, _>, _>>()?;
        let body = request
            .body()
            .and_then(reqwest::Body::as_bytes)
            .unwrap_or_default();
        let body = std::str::from_utf8(body).map_err(|_| McpClientError::Configuration)?;
        let value=callback(&self.bridge,PendingKind::McpOAuthHttp,
            json!({"url":request.url().as_str(),"method":request.method().as_str(),"headers":headers,"body":body,"redirect":"error"})).await?;
        let status = value["status"]
            .as_u64()
            .filter(|status| (100..=599).contains(status))
            .ok_or(McpClientError::Configuration)?;
        let body = value["body"]
            .as_str()
            .ok_or(McpClientError::Configuration)?;
        if body.len() > 4_194_304 {
            return Err(McpClientError::Configuration);
        }
        let mut response = http::Response::builder().status(status as u16);
        if let Some(headers) = value.get("headers") {
            for (name, value) in headers.as_object().ok_or(McpClientError::Configuration)? {
                response =
                    response.header(name, value.as_str().ok_or(McpClientError::Configuration)?);
            }
        }
        response
            .body(body.as_bytes().to_vec())
            .map(Into::into)
            .map_err(|_| McpClientError::Configuration)
    }
}

async fn provider(
    alias: &str,
    options: &str,
    bridge: Arc<JsBridge>,
) -> Result<client_ports::OAuthTransport> {
    let mut value = agentkit_contracts::parse_javascript_json(options)
        .map_err(|_| errors::invalid("Invalid OAuth options"))?;
    let use_http = value
        .as_object_mut()
        .and_then(|value| value.remove("httpCallback"));
    let use_http = match use_http {
        None => false,
        Some(Value::Bool(value)) => value,
        _ => return Err(errors::invalid("Invalid OAuth HTTP port selection")),
    };
    let options: ManagedOAuthOptions =
        serde_json::from_value(value).map_err(|_| errors::invalid("Invalid OAuth options"))?;
    let secrets = Arc::new(SecretPort {
        bridge: bridge.clone(),
    });
    let browser = Arc::new(BrowserPort {
        bridge: bridge.clone(),
        alias: alias.into(),
    });
    let http = Arc::new(HttpPort { bridge });
    let provider = if use_http {
        ManagedOAuthProvider::with_http(options, secrets, browser, http.clone()).await
    } else {
        ManagedOAuthProvider::new(options, secrets, browser).await
    };
    let provider = provider.map(Arc::new).map_err(map_client)?;
    Ok(client_ports::OAuthTransport {
        provider,
        executor: use_http.then_some(http as Arc<dyn agentkit_mcp::McpHttpExecutor>),
    })
}

#[napi]
impl NativeStore {
    #[napi(js_name = "mcpConnectWithOAuth", catch_unwind)]
    pub async fn mcp_connect_with_oauth(
        &self,
        config_json: String,
        oauth_options_json: String,
        deliver: ThreadsafeFunction<String>,
    ) -> Result<String> {
        let value = agentkit_contracts::parse_javascript_json(&config_json)
            .map_err(|_| errors::invalid("Invalid MCP config"))?;
        let alias = value["alias"]
            .as_str()
            .ok_or_else(|| errors::invalid("Invalid MCP alias"))?;
        let bridge = Arc::new(JsBridge::new(deliver));
        let result = async {
            let oauth = provider(alias, &oauth_options_json, bridge.clone()).await?;
            self.connect_mcp(config_json.clone(), Some(bridge.clone()), Some(oauth))
                .await
        }
        .await;
        if result.is_err() {
            bridge.start_closing();
            bridge.mark_closed();
        }
        result
    }
    #[napi(js_name = "mcpCompleteOAuth", catch_unwind)]
    pub async fn mcp_complete_oauth(
        &self,
        oauth_options_json: String,
        callback_url: String,
        deliver: ThreadsafeFunction<String>,
    ) -> Result<()> {
        self.oauth_operation(oauth_options_json, Some(callback_url), deliver)
            .await
    }
    #[napi(js_name = "mcpRevokeOAuth", catch_unwind)]
    pub async fn mcp_revoke_oauth(
        &self,
        oauth_options_json: String,
        deliver: ThreadsafeFunction<String>,
    ) -> Result<()> {
        self.oauth_operation(oauth_options_json, None, deliver)
            .await
    }
}

impl NativeStore {
    async fn oauth_operation(
        &self,
        options: String,
        callback_url: Option<String>,
        deliver: ThreadsafeFunction<String>,
    ) -> Result<()> {
        let alias = format!("@oauth:{}", uuid::Uuid::new_v4());
        let bridge = Arc::new(JsBridge::new(deliver));
        let binding = client_ports::CallbackBinding {
            bridge: bridge.clone(),
            elicitation: false,
        };
        let candidate = self.admit_connect(&alias, Some(&binding), None).await?;
        let result=async {
            let oauth=provider(&alias,&options,bridge.clone()).await?.provider;
            tokio::select! {
                biased;
                _=candidate.token.cancelled()=>Err(errors::closed("the OAuth operation was cancelled")),
                result=async {match callback_url {Some(url)=>oauth.complete(&url).await,None=>oauth.disconnect().await}}=>result.map_err(map_client),
            }
        }.await;
        self.abandon_connect(&alias, &candidate.id).await;
        result
    }
}
