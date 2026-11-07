use crate::{McpClientError, oauth_http::CheckedHttp, oauth_store::BoundStore};
use async_trait::async_trait;
use rmcp::transport::auth::{AuthorizationManager, OAuthClientConfig, OAuthHttpClient};
use serde::{Deserialize, Serialize};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::Mutex;
use tracing::instrument::WithSubscriber;
use url::Url;

#[async_trait]
pub trait OAuthSecrets: Send + Sync {
    async fn get(&self, key: &str) -> Result<Option<String>, McpClientError>;
    async fn set(&self, key: &str, value: &str) -> Result<(), McpClientError>;
    async fn delete(&self, key: &str) -> Result<(), McpClientError>;
}

#[async_trait]
pub trait OAuthBrowser: Send + Sync {
    async fn redirect(&self, url: &str) -> Result<(), McpClientError>;
}

/// Immutable identity supplied by the embedding application's account selection.
/// Credentials and discovered client secrets are written only through OAuthSecrets.
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ManagedOAuthOptions {
    pub app_namespace: String,
    pub generation: u64,
    pub connection_id: String,
    pub account_id: String,
    pub resource_url: String,
    pub redirect_url: String,
    pub allowed_origins: Vec<String>,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub client_name: String,
    pub scopes: Vec<String>,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct SavedClient {
    pub id: String,
    pub secret: Option<String>,
    pub issuer: String,
}

struct State {
    manager: AuthorizationManager,
    prepared: bool,
}

pub struct ManagedOAuthProvider {
    options: ManagedOAuthOptions,
    state: Mutex<State>,
    store: BoundStore,
    http: Arc<CheckedHttp>,
    browser: Arc<dyn OAuthBrowser>,
    revoked: Arc<AtomicBool>,
}

impl ManagedOAuthProvider {
    pub(crate) fn scrub(&self, raw: &str) -> Result<String, McpClientError> {
        self.store.scrub(raw)
    }
    pub async fn new(
        options: ManagedOAuthOptions,
        secrets: Arc<dyn OAuthSecrets>,
        browser: Arc<dyn OAuthBrowser>,
    ) -> Result<Self, McpClientError> {
        let http = Arc::new(crate::oauth_http::Network::new()?);
        Self::with_http(options, secrets, browser, http).await
    }

    /// The injected HTTP port remains behind the same origin, redirect and resource checks.
    pub async fn with_http(
        mut options: ManagedOAuthOptions,
        secrets: Arc<dyn OAuthSecrets>,
        browser: Arc<dyn OAuthBrowser>,
        http: Arc<dyn OAuthHttpClient>,
    ) -> Result<Self, McpClientError> {
        let resource =
            Url::parse(&options.resource_url).map_err(|_| McpClientError::Configuration)?;
        let redirect =
            Url::parse(&options.redirect_url).map_err(|_| McpClientError::Configuration)?;
        if options.app_namespace.is_empty()
            || options.generation == 0
            || options.generation > 9_007_199_254_740_991
            || options.connection_id.is_empty()
            || options.account_id.is_empty()
            || resource.query().is_some()
            || redirect.query().is_some()
            || redirect.fragment().is_some()
            || !redirect.username().is_empty()
            || redirect.password().is_some()
        {
            return Err(McpClientError::Configuration);
        }
        options.resource_url = resource.to_string();
        options.redirect_url = redirect.to_string();
        options.allowed_origins.sort();
        options.allowed_origins.dedup();
        let http = Arc::new(CheckedHttp::new(&options, http)?);
        let store = BoundStore::new(&options, secrets)?;
        let revoked = store.revocation_flag();
        let mut manager =
            AuthorizationManager::new_with_oauth_http_client(&options.resource_url, http.clone())
                .await
                .map_err(|_| McpClientError::Configuration)?;
        manager.set_credential_store(store.clone());
        manager.set_state_store(store.clone());
        Ok(Self {
            options,
            state: Mutex::new(State {
                manager,
                prepared: false,
            }),
            store,
            http,
            browser,
            revoked,
        })
    }

    pub fn resource_url(&self) -> &str {
        &self.options.resource_url
    }

    pub async fn access_token(&self) -> Result<String, McpClientError> {
        // The pinned SDK logs authorization codes at DEBUG; never expose those
        // events even when the embedding host enables verbose transport traces.
        self.token_inner(false)
            .with_subscriber(tracing::Dispatch::none())
            .await
    }

    pub async fn authorize(&self) -> Result<(), McpClientError> {
        self.token_inner(true)
            .with_subscriber(tracing::Dispatch::none())
            .await
            .map(|_| ())
    }

    async fn token_inner(&self, interactive: bool) -> Result<String, McpClientError> {
        self.live()?;
        let _operation = self.store.operation_guard().await;
        let mut state = self.state.lock().await;
        self.live()?;
        self.prepare(&mut state).await?;
        match state.manager.get_access_token().await {
            Ok(token) => {
                self.live()?;
                Ok(token)
            }
            Err(rmcp::transport::auth::AuthError::AuthorizationRequired) => {
                if !interactive {
                    return Err(McpClientError::AuthRequired);
                }
                let scopes = self
                    .options
                    .scopes
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>();
                let url = state
                    .manager
                    .get_authorization_url(&scopes)
                    .await
                    .map_err(|_| McpClientError::AuthRequired)?;
                self.http.check(&url)?;
                self.live()?;
                self.browser.redirect(&url).await?;
                Err(McpClientError::AuthRequired)
            }
            Err(_) => Err(McpClientError::AuthRequired),
        }
    }

    async fn prepare(&self, state: &mut State) -> Result<(), McpClientError> {
        if state.prepared {
            return Ok(());
        }
        let metadata = state
            .manager
            .resolve_metadata()
            .await
            .map_err(|_| McpClientError::AuthRequired)?
            .metadata;
        self.http.validate_metadata(&metadata)?;
        let issuer = metadata
            .issuer
            .clone()
            .ok_or(McpClientError::AuthRequired)?;
        self.store.set_issuer(&issuer);
        state.manager.set_metadata(metadata);
        let saved = self
            .store
            .read::<SavedClient>("client")
            .await
            .map_err(|_| McpClientError::AuthRequired)?;
        let config = if let Some(id) = &self.options.client_id {
            if saved
                .as_ref()
                .is_some_and(|saved| saved.id != *id || saved.issuer != issuer)
            {
                return Err(McpClientError::AuthRequired);
            }
            let mut config = OAuthClientConfig::new(id, &self.options.redirect_url)
                .with_scopes(self.options.scopes.clone());
            config.client_secret = self.options.client_secret.clone();
            config
        } else if let Some(saved) = saved {
            if saved.issuer != issuer {
                return Err(McpClientError::AuthRequired);
            }
            let mut config = OAuthClientConfig::new(saved.id, &self.options.redirect_url)
                .with_scopes(self.options.scopes.clone());
            config.client_secret = saved.secret;
            config
        } else {
            let scopes = self
                .options
                .scopes
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>();
            state
                .manager
                .register_client(
                    &self.options.client_name,
                    &self.options.redirect_url,
                    &scopes,
                )
                .await
                .map_err(|_| McpClientError::AuthRequired)?
        };
        self.store
            .write(
                "client",
                &SavedClient {
                    id: config.client_id.clone(),
                    secret: config.client_secret.clone(),
                    issuer,
                },
            )
            .await
            .map_err(|_| McpClientError::AuthRequired)?;
        state
            .manager
            .configure_client(config)
            .map_err(|_| McpClientError::AuthRequired)?;
        state.prepared = true;
        Ok(())
    }

    pub async fn complete(&self, callback_url: &str) -> Result<(), McpClientError> {
        self.complete_inner(callback_url)
            .with_subscriber(tracing::Dispatch::none())
            .await
    }

    async fn complete_inner(&self, callback_url: &str) -> Result<(), McpClientError> {
        self.live()?;
        let callback = Url::parse(callback_url).map_err(|_| McpClientError::AuthRequired)?;
        let expected =
            Url::parse(&self.options.redirect_url).map_err(|_| McpClientError::Configuration)?;
        if callback.scheme() != expected.scheme()
            || callback.host_str() != expected.host_str()
            || callback.port() != expected.port()
            || callback.path() != expected.path()
            || callback.fragment().is_some()
            || !callback.username().is_empty()
            || callback.password().is_some()
        {
            return Err(McpClientError::AuthRequired);
        }
        let mut params = std::collections::BTreeMap::new();
        for (key, value) in callback.query_pairs() {
            if params
                .insert(key.into_owned(), value.into_owned())
                .is_some()
            {
                return Err(McpClientError::AuthRequired);
            }
        }
        let code = params
            .get("code")
            .filter(|value| !value.is_empty())
            .ok_or(McpClientError::AuthRequired)?;
        let csrf = params
            .get("state")
            .filter(|value| !value.is_empty())
            .ok_or(McpClientError::AuthRequired)?;
        if params.contains_key("error") {
            return Err(McpClientError::AuthRequired);
        }
        let _operation = self.store.operation_guard().await;
        let mut state = self.state.lock().await;
        self.live()?;
        self.prepare(&mut state).await?;
        state
            .manager
            .exchange_code_for_token_with_issuer(code, csrf, params.get("iss").map(String::as_str))
            .await
            .map_err(|_| McpClientError::AuthRequired)?;
        self.live()
    }

    pub async fn disconnect(&self) -> Result<(), McpClientError> {
        self.revoked.store(true, Ordering::Release);
        self.store.persist_revocation().await?;
        let _operation = self.store.operation_guard().await;
        let _state = self.state.lock().await;
        for key in ["credentials", "client", "state"] {
            self.store.delete(key).await?;
        }
        Ok(())
    }

    pub(crate) async fn unauthorized(
        &self,
        used_token: Option<&str>,
    ) -> Result<(), McpClientError> {
        let _operation = self.store.operation_guard().await;
        let current = self
            .store
            .read::<serde_json::Value>("credentials")
            .await
            .map_err(|_| McpClientError::AuthRequired)?;
        if current
            .as_ref()
            .and_then(|value| value["token_response"]["access_token"].as_str())
            != used_token
        {
            return Ok(());
        }
        self.store
            .expire()
            .await
            .map_err(|_| McpClientError::AuthRequired)
    }

    fn live(&self) -> Result<(), McpClientError> {
        if self.revoked.load(Ordering::Acquire) {
            Err(McpClientError::AuthRequired)
        } else {
            Ok(())
        }
    }
}
