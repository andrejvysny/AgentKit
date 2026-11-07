use super::*;
use crate::jsbridge::{JsBridge, PendingKind};
use agentkit_mcp::{
    ClientOptions, DefaultConnector, ElicitationHandler, McpConnection, McpConnector,
};
use std::sync::RwLock;

pub(super) type CallbackMap = Arc<RwLock<HashMap<String, CallbackBinding>>>;
#[derive(Clone)]
pub(super) struct CallbackBinding {
    pub bridge: Arc<JsBridge>,
    pub elicitation: bool,
}
pub(super) type OAuthMap = Arc<RwLock<HashMap<String, OAuthTransport>>>;
#[derive(Clone)]
pub(super) struct OAuthTransport {
    pub provider: Arc<agentkit_mcp::ManagedOAuthProvider>,
    pub executor: Option<Arc<dyn agentkit_mcp::McpHttpExecutor>>,
}

pub(super) fn registry(shared: Arc<std::sync::Mutex<RuntimeState>>) -> Result<Registry> {
    let callbacks = CallbackMap::default();
    let oauth = OAuthMap::default();
    let connector = Arc::new(CallbackConnector {
        callbacks: callbacks.clone(),
        oauth: oauth.clone(),
    });
    let manager = McpClientManager::with_ports(
        Vec::new(),
        Arc::new(client_runtime::Vault::new(shared.clone())),
        connector,
        Arc::new(agentkit_mcp::SystemClock),
    )
    .map_err(map_client)?;
    let manager = Arc::new(manager);
    shared
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .manager = Some(Arc::downgrade(&manager));
    Ok(Registry {
        shared,
        manager,
        callbacks,
        oauth,
        connecting: HashMap::new(),
        configs: BTreeMap::new(),
        tools: HashMap::new(),
        registrations: HashMap::new(),
        pending: HashMap::new(),
        cancelled: HashSet::new(),
    })
}

pub(super) fn model_capabilities(config: &str) -> Result<agentkit_mcp::ModelContentCapabilities> {
    let value = agentkit_contracts::parse_javascript_json(config)
        .map_err(|_| errors::invalid("Invalid MCP config"))?;
    let Some(value) = value.get("modelContentCapabilities") else {
        return Ok(Default::default());
    };
    let object = value
        .as_object()
        .ok_or_else(|| errors::invalid("Invalid modelContentCapabilities"))?;
    if object
        .iter()
        .any(|(key, value)| !matches!(key.as_str(), "image" | "audio") || !value.is_boolean())
    {
        return Err(errors::invalid("Invalid modelContentCapabilities"));
    }
    Ok(agentkit_mcp::ModelContentCapabilities {
        image: value["image"].as_bool().unwrap_or(false),
        audio: value["audio"].as_bool().unwrap_or(false),
    })
}

pub(super) struct CallbackConnector {
    pub callbacks: CallbackMap,
    pub oauth: OAuthMap,
}

#[async_trait]
impl McpConnector for CallbackConnector {
    async fn connect(
        &self,
        transport: TransportConfig,
        options: ClientOptions,
        cancellation: CancellationToken,
    ) -> std::result::Result<Arc<dyn McpConnection>, McpClientError> {
        DefaultConnector
            .connect(transport, options, cancellation)
            .await
    }
    async fn connect_named(
        &self,
        alias: &str,
        transport: TransportConfig,
        mut options: ClientOptions,
        cancellation: CancellationToken,
    ) -> std::result::Result<Arc<dyn McpConnection>, McpClientError> {
        let bridge = self
            .callbacks
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .get(alias)
            .cloned();
        if let Some(binding) = bridge.filter(|binding| binding.elicitation) {
            options.elicitation = Some(Arc::new(JsElicitation {
                alias: alias.into(),
                bridge: binding.bridge,
            }));
        }
        let oauth = self
            .oauth
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .get(alias)
            .cloned();
        if let Some(oauth) = oauth {
            options.http_executor = oauth.executor;
            let TransportConfig::Http { url, headers } = transport else {
                return Err(McpClientError::Configuration);
            };
            let mut values = http::HeaderMap::new();
            for (key, value) in headers {
                values.insert(
                    http::HeaderName::try_from(key).map_err(|_| McpClientError::Configuration)?,
                    http::HeaderValue::try_from(value)
                        .map_err(|_| McpClientError::Configuration)?,
                );
            }
            return agentkit_mcp::McpClient::http_with_oauth(&url, values, options, oauth.provider)
                .await
                .map(|client| Arc::new(client) as Arc<dyn McpConnection>);
        }
        self.connect(transport, options, cancellation).await
    }
}

struct JsElicitation {
    alias: String,
    bridge: Arc<JsBridge>,
}

#[async_trait]
impl ElicitationHandler for JsElicitation {
    async fn elicit(
        &self,
        params: Value,
        cancellation: CancellationToken,
    ) -> std::result::Result<agentkit_mcp::model::ElicitResult, agentkit_mcp::model::ErrorData>
    {
        use agentkit_mcp::model::{ElicitResult, ElicitationAction, ErrorData};
        let request = json!({"alias":self.alias,"params":params});
        let call_id = uuid::Uuid::new_v4().to_string();
        let value = self
            .bridge
            .clone()
            .request(
                PendingKind::McpElicitation,
                &call_id,
                &request,
                &cancellation,
            )
            .await
            .map_err(|_| ErrorData::internal_error("MCP elicitation callback failed", None))?;
        let action = match value["action"].as_str() {
            Some("accept") => ElicitationAction::Accept,
            Some("decline") => ElicitationAction::Decline,
            Some("cancel") => ElicitationAction::Cancel,
            _ => {
                return Err(ErrorData::invalid_params(
                    "Invalid MCP elicitation action",
                    None,
                ));
            }
        };
        let mut result = ElicitResult::new(action);
        result.content = value.get("content").cloned();
        Ok(result)
    }
}

pub(super) fn close_callback(callbacks: &CallbackMap, alias: &str) {
    if let Some(bridge) = callbacks
        .write()
        .unwrap_or_else(|error| error.into_inner())
        .remove(alias)
    {
        bridge.bridge.start_closing();
        bridge.bridge.mark_closed();
    }
}

/// Allocate before connecting; a failed attempt consumes a fence, never reuses one.
pub(super) async fn next_registration(
    store: &agentkit_store::SqliteStore,
    alias: &str,
) -> Result<u64> {
    for _ in 0..16 {
        let previous = store
            .get_setting("mcp-lifecycle", alias)
            .await
            .map_err(errors::store)?;
        let current = match &previous {
            None => 0,
            Some(value) => value
                .as_u64()
                .ok_or_else(|| errors::invalid("Invalid persisted MCP lifecycle generation"))?,
        };
        let next = current
            .checked_add(1)
            .filter(|value| *value <= 9_007_199_254_740_991)
            .ok_or_else(|| errors::invalid("MCP lifecycle generation exhausted"))?;
        if store
            .compare_and_set_setting("mcp-lifecycle", alias, previous, json!(next))
            .await
            .map_err(errors::store)?
        {
            return Ok(next);
        }
    }
    Err(errors::invalid(
        "MCP lifecycle generation changed concurrently",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn lifecycle_fences_survive_restart_and_reject_corrupt_counters() {
        let directory =
            std::env::temp_dir().join(format!("agentkit-mcp-fence-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("store.sqlite");
        let first = agentkit_store::SqliteStore::open(&path, 8).unwrap();
        assert_eq!(next_registration(&first, "docs").await.unwrap(), 1);
        drop(first);
        let second = agentkit_store::SqliteStore::open(&path, 8).unwrap();
        assert_eq!(next_registration(&second, "docs").await.unwrap(), 2);
        second
            .update_setting("mcp-lifecycle", "docs", Value::Null, |_| json!("corrupt"))
            .await
            .unwrap();
        assert!(next_registration(&second, "docs").await.is_err());
        assert_eq!(
            second.get_setting("mcp-lifecycle", "docs").await.unwrap(),
            Some(json!("corrupt"))
        );
        drop(second);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
