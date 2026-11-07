use agentkit_mcp::{
    ManagedOAuthOptions, ManagedOAuthProvider, McpClientError, OAuthBrowser, OAuthSecrets,
};
use async_trait::async_trait;
use rmcp::transport::auth::{OAuthHttpClient, OAuthHttpClientFuture, OAuthHttpRequest};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use url::Url;

#[derive(Default)]
struct Secrets(Mutex<BTreeMap<String, String>>);
#[async_trait]
impl OAuthSecrets for Secrets {
    async fn get(&self, key: &str) -> Result<Option<String>, McpClientError> {
        Ok(self.0.lock().unwrap().get(key).cloned())
    }
    async fn set(&self, key: &str, value: &str) -> Result<(), McpClientError> {
        self.0.lock().unwrap().insert(key.into(), value.into());
        Ok(())
    }
    async fn delete(&self, key: &str) -> Result<(), McpClientError> {
        self.0.lock().unwrap().remove(key);
        Ok(())
    }
}
#[derive(Default)]
struct Browser(Mutex<Vec<String>>);
#[async_trait]
impl OAuthBrowser for Browser {
    async fn redirect(&self, url: &str) -> Result<(), McpClientError> {
        self.0.lock().unwrap().push(url.into());
        Ok(())
    }
}
#[derive(Default)]
struct Network {
    calls: Mutex<Vec<String>>,
    exchanges: AtomicUsize,
    refreshes: AtomicUsize,
    resource_override: Option<String>,
    issuer_override: Option<String>,
    revoke_during_refresh: Option<Arc<std::sync::atomic::AtomicBool>>,
}
impl OAuthHttpClient for Network {
    fn execute(&self, request: OAuthHttpRequest) -> OAuthHttpClientFuture<'_> {
        Box::pin(async move {
            let uri = Url::parse(&request.request.uri().to_string()).unwrap();
            self.calls.lock().unwrap().push(uri.to_string());
            let mut builder = http::Response::builder().header("content-type", "application/json");
            let value = if uri.host_str() == Some("resource.test") && uri.path() == "/mcp" {
                builder=builder.status(401).header("www-authenticate", "Bearer resource_metadata=\"https://resource.test/.well-known/oauth-protected-resource/mcp\"");
                json!({})
            } else if uri.host_str() == Some("resource.test") {
                json!({"resource":self.resource_override.as_deref().unwrap_or("https://resource.test/mcp"),"authorization_servers":["https://issuer.test"]})
            } else if uri.path().contains(".well-known") {
                json!({"issuer":self.issuer_override.as_deref().unwrap_or("https://issuer.test"),
                    "authorization_endpoint":"https://issuer.test/authorize","token_endpoint":"https://issuer.test/token",
                    "registration_endpoint":"https://issuer.test/register","response_types_supported":["code"],
                    "grant_types_supported":["authorization_code","refresh_token"],"code_challenge_methods_supported":["S256"],
                    "authorization_response_iss_parameter_supported":true})
            } else if uri.path() == "/register" {
                builder = builder.status(201);
                json!({"client_id":"dynamic-client","client_secret":"dynamic-secret","redirect_uris":["onemind://oauth/callback"]})
            } else if uri.path() == "/token" {
                let params = url::form_urlencoded::parse(request.request.body())
                    .into_owned()
                    .collect::<BTreeMap<_, _>>();
                assert_eq!(
                    params.get("resource").map(String::as_str),
                    Some("https://resource.test/mcp")
                );
                if params.get("grant_type").map(String::as_str) == Some("refresh_token") {
                    self.refreshes.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    if let Some(allowed) = &self.revoke_during_refresh {
                        allowed.store(false, Ordering::SeqCst);
                    }
                    json!({"access_token":"access-refresh","token_type":"Bearer","expires_in":3600})
                } else {
                    self.exchanges.fetch_add(1, Ordering::SeqCst);
                    assert!(
                        params
                            .get("code_verifier")
                            .is_some_and(|value| !value.is_empty())
                    );
                    json!({"access_token":"access-one","refresh_token":"refresh-one","token_type":"Bearer","expires_in":3600})
                }
            } else {
                return Err(McpClientError::Configuration.into());
            };
            Ok(builder.body(serde_json::to_vec(&value).unwrap()).unwrap())
        })
    }
}

fn options() -> ManagedOAuthOptions {
    ManagedOAuthOptions {
        app_namespace: uuid::Uuid::new_v4().to_string(),
        generation: 1,
        connection_id: "connection".into(),
        account_id: "trusted-account".into(),
        resource_url: "https://resource.test/mcp".into(),
        redirect_url: "onemind://oauth/callback".into(),
        allowed_origins: vec!["https://resource.test".into(), "https://issuer.test".into()],
        client_id: None,
        client_secret: None,
        client_name: "Test".into(),
        scopes: vec!["read".into()],
    }
}
async fn provider(
    options: ManagedOAuthOptions,
    secrets: Arc<Secrets>,
    browser: Arc<Browser>,
    network: Arc<Network>,
) -> ManagedOAuthProvider {
    ManagedOAuthProvider::with_http(options, secrets, browser, network)
        .await
        .unwrap()
}
fn callback(browser: &Browser) -> String {
    let url = Url::parse(browser.0.lock().unwrap().last().unwrap()).unwrap();
    let params = url.query_pairs().into_owned().collect::<BTreeMap<_, _>>();
    assert_eq!(
        params.get("code_challenge_method").map(String::as_str),
        Some("S256")
    );
    assert!(
        params
            .get("code_challenge")
            .is_some_and(|value| value.len() >= 43)
    );
    let mut callback = Url::parse("onemind://oauth/callback").unwrap();
    callback
        .query_pairs_mut()
        .append_pair("code", "authorization-code")
        .append_pair("state", &params["state"])
        .append_pair("iss", "https://issuer.test");
    callback.to_string()
}
async fn authorize(provider: &ManagedOAuthProvider, browser: &Browser) {
    assert_eq!(
        provider.authorize().await.unwrap_err(),
        McpClientError::AuthRequired
    );
    provider.complete(&callback(browser)).await.unwrap();
}

#[tokio::test]
async fn pkce_state_issuer_binding_and_restart_preserve_only_secret_store_credentials() {
    let options = options();
    let secrets = Arc::new(Secrets::default());
    let browser = Arc::new(Browser::default());
    let network = Arc::new(Network::default());
    let first = provider(
        options.clone(),
        secrets.clone(),
        browser.clone(),
        network.clone(),
    )
    .await;
    assert_eq!(
        first.authorize().await.unwrap_err(),
        McpClientError::AuthRequired
    );
    let valid = callback(&browser);
    assert!(
        first
            .complete(&valid.replace("state=", "state=forged"))
            .await
            .is_err()
    );
    assert!(
        first
            .complete(&valid.replace("issuer.test", "evil.test"))
            .await
            .is_err()
    );
    assert_eq!(network.exchanges.load(Ordering::SeqCst), 0);
    first.complete(&valid).await.unwrap();
    assert!(first.complete(&valid).await.is_err());
    assert_eq!(first.access_token().await.unwrap(), "access-one");
    assert_eq!(network.exchanges.load(Ordering::SeqCst), 1);
    let entries = secrets.0.lock().unwrap().clone();
    assert!(
        entries
            .values()
            .any(|value| value.contains("dynamic-secret"))
    );
    assert!(entries.values().any(|value| value.contains("access-one")));
    assert!(
        entries
            .keys()
            .all(|key| key.starts_with(&format!("oauth/{}/", options.app_namespace)))
    );
    drop(first);
    let restarted = provider(options, secrets, Arc::new(Browser::default()), network).await;
    assert_eq!(restarted.access_token().await.unwrap(), "access-one");
}

#[tokio::test]
async fn distinct_handles_share_one_refresh_and_preserve_omitted_refresh_token() {
    let options = options();
    let secrets = Arc::new(Secrets::default());
    let browser = Arc::new(Browser::default());
    let network = Arc::new(Network::default());
    let first = provider(
        options.clone(),
        secrets.clone(),
        browser.clone(),
        network.clone(),
    )
    .await;
    authorize(&first, &browser).await;
    {
        let mut entries = secrets.0.lock().unwrap();
        let value = entries
            .iter_mut()
            .find(|(key, _)| key.ends_with("credentials"))
            .unwrap()
            .1;
        let mut saved: Value = serde_json::from_str(value).unwrap();
        saved["value"]["token_received_at"] = json!(0);
        *value = saved.to_string();
    }
    let second = provider(
        options,
        secrets.clone(),
        Arc::new(Browser::default()),
        network.clone(),
    )
    .await;
    let (left, right) = tokio::join!(first.access_token(), second.access_token());
    assert_eq!(left.unwrap(), "access-refresh");
    assert_eq!(right.unwrap(), "access-refresh");
    assert_eq!(network.refreshes.load(Ordering::SeqCst), 1);
    assert!(
        secrets
            .0
            .lock()
            .unwrap()
            .values()
            .any(|value| value.contains("refresh-one"))
    );
}

#[tokio::test]
async fn old_generation_disconnect_never_deletes_replacement_credentials() {
    let options = options();
    let secrets = Arc::new(Secrets::default());
    let browser = Arc::new(Browser::default());
    let network = Arc::new(Network::default());
    let old = provider(
        options.clone(),
        secrets.clone(),
        browser.clone(),
        network.clone(),
    )
    .await;
    authorize(&old, &browser).await;
    let mut next = options.clone();
    next.generation = 2;
    let replacement = provider(next, secrets.clone(), browser.clone(), network.clone()).await;
    authorize(&replacement, &browser).await;
    let replacement_entries = secrets
        .0
        .lock()
        .unwrap()
        .iter()
        .filter(|(key, _)| key.contains("/2/"))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect::<BTreeMap<_, _>>();
    old.disconnect().await.unwrap();
    for (key, value) in replacement_entries {
        assert_eq!(secrets.0.lock().unwrap().get(&key), Some(&value));
    }
    assert_eq!(replacement.access_token().await.unwrap(), "access-one");
    drop(old);
    let revoked = provider(options, secrets, browser, network).await;
    assert!(
        revoked.access_token().await.is_err(),
        "durable revocation must survive a new handle"
    );
}

#[tokio::test]
async fn resource_or_issuer_discovery_drift_never_opens_a_browser_or_exchanges_tokens() {
    for network in [
        Network {
            resource_override: Some("https://evil.test/mcp".into()),
            ..Default::default()
        },
        Network {
            issuer_override: Some("https://evil.test".into()),
            ..Default::default()
        },
    ] {
        let network = Arc::new(network);
        let browser = Arc::new(Browser::default());
        let provider = provider(
            options(),
            Arc::new(Secrets::default()),
            browser.clone(),
            network.clone(),
        )
        .await;
        assert!(provider.authorize().await.is_err());
        assert!(browser.0.lock().unwrap().is_empty());
        assert_eq!(network.exchanges.load(Ordering::SeqCst), 0);
        assert!(
            !network
                .calls
                .lock()
                .unwrap()
                .iter()
                .any(|url| url.starts_with("https://evil.test"))
        );
    }
}

#[derive(Default)]
struct Resource {
    effects: AtomicUsize,
    unauthorized: bool,
    echo: Option<&'static str>,
}
#[async_trait]
impl agentkit_mcp::McpHttpExecutor for Resource {
    async fn execute(
        &self,
        request: reqwest::Request,
    ) -> Result<reqwest::Response, McpClientError> {
        assert_eq!(request.url().as_str(), "https://resource.test/mcp");
        assert!(request.headers().get("authorization").is_some());
        let body: Value =
            serde_json::from_slice(request.body().and_then(reqwest::Body::as_bytes).unwrap())
                .unwrap();
        let id = &body["id"];
        let (status, value) = match body["method"].as_str() {
            Some("initialize") => (
                200,
                json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"oauth-resource","version":"1"}}}),
            ),
            Some("tools/call") => {
                self.effects.fetch_add(1, Ordering::SeqCst);
                if self.unauthorized {
                    (
                        401,
                        json!({"jsonrpc":"2.0","id":id,"error":{"code":-32001,"message":"denied"}}),
                    )
                } else if self.echo == Some("error") {
                    (
                        500,
                        json!({"jsonrpc":"2.0","id":id,"error":{"code":-32001,"message":request.headers()["authorization"].to_str().unwrap()}}),
                    )
                } else {
                    let text = if self.echo.is_some() {
                        request.headers()["authorization"].to_str().unwrap()
                    } else {
                        "done"
                    };
                    (
                        200,
                        json!({"jsonrpc":"2.0","id":id,"result":{"content":[{"type":"text","text":text}]}}),
                    )
                }
            }
            _ => (202, Value::Null),
        };
        if self.echo == Some("sse") && body["method"] == "tools/call" {
            let event = format!("data: {}\n\n", serde_json::to_string(&value).unwrap());
            let chunks = event
                .as_bytes()
                .chunks(3)
                .map(|chunk| Ok::<_, std::io::Error>(chunk.to_vec()))
                .collect::<Vec<_>>();
            return Ok(http::Response::builder()
                .status(status)
                .header("content-type", "text/event-stream")
                .body(reqwest::Body::wrap_stream(futures::stream::iter(chunks)))
                .unwrap()
                .into());
        }
        Ok(http::Response::builder()
            .status(status)
            .header("content-type", "application/json")
            .body(serde_json::to_vec(&value).unwrap())
            .unwrap()
            .into())
    }
}

#[tokio::test]
async fn oauth_echoed_credentials_are_redacted_before_json_sse_and_errors_surface() {
    for mode in ["json", "sse", "error"] {
        let browser = Arc::new(Browser::default());
        let oauth = Arc::new(
            provider(
                options(),
                Arc::new(Secrets::default()),
                browser.clone(),
                Arc::new(Network::default()),
            )
            .await,
        );
        authorize(&oauth, &browser).await;
        let token = oauth.access_token().await.unwrap();
        let resource = Arc::new(Resource {
            echo: Some(mode),
            ..Default::default()
        });
        let options = agentkit_mcp::ClientOptions {
            http_executor: Some(resource),
            ..Default::default()
        };
        let client = agentkit_mcp::McpClient::http_with_oauth(
            "https://resource.test/mcp",
            Default::default(),
            options,
            oauth,
        )
        .await
        .unwrap();
        let response = client
            .call_tool("write", Default::default(), Default::default())
            .await;
        let text = match response {
            Ok(value) => serde_json::to_string(&value).unwrap(),
            Err(McpClientError::Remote(message)) => message,
            Err(error) => panic!("unexpected OAuth echo result: {error}"),
        };
        assert!(!text.contains(&token));
        assert!(text.contains("[REDACTED]"));
        client.close().await.unwrap();
    }
}

#[derive(Debug)]
struct Guard(Arc<std::sync::atomic::AtomicBool>);
#[async_trait]
impl agentkit_core::DispatchGuard for Guard {
    async fn check(&self) -> Result<(), agentkit_core::CoreError> {
        if self.0.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err(agentkit_core::CoreError::new(
                "revoked",
                "Host authority revoked",
            ))
        }
    }
    fn fence(&self) -> agentkit_core::DispatchFence {
        agentkit_core::DispatchFence {
            task_id: "task".into(),
            attempt_id: "attempt".into(),
            fencing_token: 1,
        }
    }
}

#[tokio::test]
async fn actual_oauth_http_adapter_never_replays_a_401_after_mutation() {
    let secrets = Arc::new(Secrets::default());
    let browser = Arc::new(Browser::default());
    let network = Arc::new(Network::default());
    let oauth = Arc::new(provider(options(), secrets, browser.clone(), network.clone()).await);
    authorize(&oauth, &browser).await;
    let resource = Arc::new(Resource {
        unauthorized: true,
        ..Default::default()
    });
    let options = agentkit_mcp::ClientOptions {
        http_executor: Some(resource.clone()),
        ..Default::default()
    };
    let client = agentkit_mcp::McpClient::http_with_oauth(
        "https://resource.test/mcp",
        Default::default(),
        options,
        oauth,
    )
    .await
    .unwrap();
    assert!(
        client
            .call_tool("write", Default::default(), Default::default())
            .await
            .is_err()
    );
    assert_eq!(resource.effects.load(Ordering::SeqCst), 1);
    assert_eq!(
        network.refreshes.load(Ordering::SeqCst),
        0,
        "401 must not refresh and replay the dispatched request"
    );
    client.close().await.unwrap();
}

#[tokio::test]
async fn authority_revoked_during_oauth_refresh_denies_the_resource_post() {
    let allowed = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let network = Arc::new(Network {
        revoke_during_refresh: Some(allowed.clone()),
        ..Default::default()
    });
    let secrets = Arc::new(Secrets::default());
    let browser = Arc::new(Browser::default());
    let oauth = Arc::new(provider(options(), secrets.clone(), browser.clone(), network).await);
    authorize(&oauth, &browser).await;
    let resource = Arc::new(Resource::default());
    let options = agentkit_mcp::ClientOptions {
        http_executor: Some(resource.clone()),
        ..Default::default()
    };
    let client = agentkit_mcp::McpClient::http_with_oauth(
        "https://resource.test/mcp",
        Default::default(),
        options,
        oauth,
    )
    .await
    .unwrap();
    {
        let mut entries = secrets.0.lock().unwrap();
        let value = entries
            .iter_mut()
            .find(|(key, _)| key.ends_with("credentials"))
            .unwrap()
            .1;
        let mut saved: Value = serde_json::from_str(value).unwrap();
        saved["value"]["token_received_at"] = json!(0);
        *value = saved.to_string();
    }
    let result = client
        .call_tool_guarded(
            "write",
            Default::default(),
            Default::default(),
            Some(Arc::new(Guard(allowed))),
        )
        .await;
    assert_eq!(result.unwrap_err(), McpClientError::Authority);
    assert_eq!(
        resource.effects.load(Ordering::SeqCst),
        0,
        "OAuth refresh await must not outlive dispatch authority"
    );
    client.close().await.unwrap();
}
