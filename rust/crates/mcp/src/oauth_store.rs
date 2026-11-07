use crate::{
    McpClientError,
    oauth::{ManagedOAuthOptions, OAuthSecrets},
};
use async_trait::async_trait;
use rmcp::transport::auth::{
    AuthError, CredentialRefreshGuard, CredentialStore, StateStore, StoredAuthorizationState,
    StoredCredentials,
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    sync::{
        Arc, LazyLock, Mutex as StdMutex, RwLock, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::Mutex;

#[derive(Clone)]
pub(crate) struct BoundStore {
    secrets: Arc<dyn OAuthSecrets>,
    prefix: String,
    binding: String,
    revoked: Arc<AtomicBool>,
    issuer: Arc<RwLock<Option<String>>>,
    locks: Arc<Locks>,
}

struct Locks {
    redaction: crate::oauth_redaction::Redaction,
    operation: Arc<Mutex<()>>,
    refresh: Arc<Mutex<()>>,
    revoked: Arc<AtomicBool>,
}
static LOCKS: LazyLock<StdMutex<std::collections::HashMap<String, Weak<Locks>>>> =
    LazyLock::new(Default::default);

fn shared_locks(key: &str) -> Arc<Locks> {
    let mut locks = LOCKS.lock().unwrap_or_else(|error| error.into_inner());
    locks.retain(|_, value| value.strong_count() > 0);
    if let Some(existing) = locks.get(key).and_then(Weak::upgrade) {
        return existing;
    }
    let value = Arc::new(Locks {
        redaction: Default::default(),
        operation: Arc::default(),
        refresh: Arc::default(),
        revoked: Arc::new(AtomicBool::new(false)),
    });
    locks.insert(key.into(), Arc::downgrade(&value));
    value
}

impl BoundStore {
    pub(crate) fn scrub(&self, raw: &str) -> Result<String, McpClientError> {
        self.locks.redaction.text(raw)
    }
    pub(crate) fn new(
        options: &ManagedOAuthOptions,
        secrets: Arc<dyn OAuthSecrets>,
    ) -> Result<Self, McpClientError> {
        let material = serde_json::to_vec(&json!({"appNamespace":options.app_namespace,"generation":options.generation,
            "connectionId":options.connection_id,"accountId":options.account_id,"resource":options.resource_url,
            "redirect":options.redirect_url,"allowedOrigins":options.allowed_origins,"clientId":options.client_id,
            "clientSecret":options.client_secret,"clientName":options.client_name,"scopes":options.scopes}))
            .map_err(|_| McpClientError::Configuration)?;
        let binding: String = Sha256::digest(material)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let encode =
            |text: &str| url::form_urlencoded::byte_serialize(text.as_bytes()).collect::<String>();
        let prefix = format!(
            "oauth/{}/{}/{}/{}/{}/",
            encode(&options.app_namespace),
            encode(&options.connection_id),
            encode(&options.account_id),
            options.generation,
            binding,
        );
        let locks = shared_locks(&prefix);
        Ok(Self {
            secrets,
            prefix,
            binding,
            revoked: locks.revoked.clone(),
            issuer: Arc::default(),
            locks,
        })
    }
    pub(crate) fn revocation_flag(&self) -> Arc<AtomicBool> {
        self.revoked.clone()
    }
    pub(crate) async fn operation_guard(&self) -> tokio::sync::OwnedMutexGuard<()> {
        self.locks.operation.clone().lock_owned().await
    }
    pub(crate) async fn persist_revocation(&self) -> Result<(), McpClientError> {
        self.secrets
            .set(&format!("{}revoked", self.prefix), "1")
            .await
    }
    async fn ensure_live(&self) -> Result<(), AuthError> {
        if self.revoked.load(Ordering::Acquire)
            || self
                .secrets
                .get(&format!("{}revoked", self.prefix))
                .await
                .map_err(|_| unavailable())?
                .is_some()
        {
            return Err(AuthError::AuthorizationRequired);
        }
        Ok(())
    }
    pub(crate) fn set_issuer(&self, issuer: &str) {
        *self
            .issuer
            .write()
            .unwrap_or_else(|error| error.into_inner()) = Some(issuer.into());
    }
    pub(crate) async fn read<T: DeserializeOwned>(
        &self,
        key: &str,
    ) -> Result<Option<T>, AuthError> {
        self.ensure_live().await?;
        let text = self
            .secrets
            .get(&format!("{}{key}", self.prefix))
            .await
            .map_err(|_| unavailable())?;
        let Some(text) = text else {
            return Ok(None);
        };
        if text.len() > 1_048_576 {
            return Err(AuthError::AuthorizationRequired);
        }
        let value: Value = agentkit_contracts::parse_javascript_json(&text)
            .map_err(|_| AuthError::AuthorizationRequired)?;
        if value["version"] != 1 || value["binding"] != self.binding || value.get("value").is_none()
        {
            return Err(AuthError::AuthorizationRequired);
        }
        self.locks
            .redaction
            .remember(&value["value"])
            .map_err(|_| unavailable())?;
        serde_json::from_value(value["value"].clone())
            .map(Some)
            .map_err(|_| AuthError::AuthorizationRequired)
    }
    pub(crate) async fn write<T: Serialize>(&self, key: &str, value: &T) -> Result<(), AuthError> {
        self.ensure_live().await?;
        let value = serde_json::to_value(value).map_err(|_| unavailable())?;
        self.locks
            .redaction
            .remember(&value)
            .map_err(|_| unavailable())?;
        let text =
            serde_json::to_string(&json!({"version":1,"binding":self.binding,"value":value}))
                .map_err(|_| unavailable())?;
        self.secrets
            .set(&format!("{}{key}", self.prefix), &text)
            .await
            .map_err(|_| unavailable())
    }
    pub(crate) async fn delete(&self, key: &str) -> Result<(), McpClientError> {
        self.secrets.delete(&format!("{}{key}", self.prefix)).await
    }
    pub(crate) async fn expire(&self) -> Result<(), AuthError> {
        if let Some(mut value) = self.read::<StoredCredentials>("credentials").await? {
            value.token_received_at = Some(0);
            <Self as CredentialStore>::save(self, value).await?;
        }
        Ok(())
    }
    fn issuer_matches(&self, issuer: &Option<String>) -> bool {
        issuer.is_some()
            && *self
                .issuer
                .read()
                .unwrap_or_else(|error| error.into_inner())
                == *issuer
    }
}
fn unavailable() -> AuthError {
    AuthError::CredentialStoreError("OAuth secret storage unavailable".into())
}

#[async_trait]
impl CredentialStore for BoundStore {
    async fn load(&self) -> Result<Option<StoredCredentials>, AuthError> {
        let raw = self.read::<Value>("credentials").await?;
        if let Some(raw) = &raw
            && !raw["token_response"].is_null()
            && raw["token_response"]["expires_in"].as_u64().is_none()
        {
            return Err(AuthError::AuthorizationRequired);
        }
        let value = raw
            .map(serde_json::from_value::<StoredCredentials>)
            .transpose()
            .map_err(|_| AuthError::AuthorizationRequired)?;
        if let Some(value) = &value
            && (!self.issuer_matches(&value.issuer)
                || (value.token_response.is_some() && value.token_received_at.is_none()))
        {
            return Err(AuthError::AuthorizationRequired);
        }
        Ok(value)
    }
    async fn save(&self, credentials: StoredCredentials) -> Result<(), AuthError> {
        if !self.issuer_matches(&credentials.issuer) {
            return Err(AuthError::AuthorizationRequired);
        }
        let mut value = serde_json::to_value(credentials).map_err(|_| unavailable())?;
        if let Some(token) = value["token_response"].as_object_mut() {
            token.entry("expires_in").or_insert(json!(3600));
        }
        self.write("credentials", &value).await
    }
    async fn clear(&self) -> Result<(), AuthError> {
        self.delete("credentials").await.map_err(|_| unavailable())
    }
    async fn acquire_refresh_guard(&self) -> Result<Option<CredentialRefreshGuard>, AuthError> {
        Ok(Some(CredentialRefreshGuard::new(
            self.locks.refresh.clone().lock_owned().await,
        )))
    }
}

#[async_trait]
impl StateStore for BoundStore {
    async fn save(&self, _csrf: &str, state: StoredAuthorizationState) -> Result<(), AuthError> {
        self.write("state", &state).await
    }
    async fn load(&self, csrf: &str) -> Result<Option<StoredAuthorizationState>, AuthError> {
        let state = self.read::<StoredAuthorizationState>("state").await?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        Ok(state.filter(|state| {
            state.csrf_token == csrf && state.created_at <= now && now - state.created_at <= 600
        }))
    }
    async fn delete(&self, csrf: &str) -> Result<(), AuthError> {
        if <Self as StateStore>::load(self, csrf).await?.is_some() {
            self.delete("state").await.map_err(|_| unavailable())?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct Secrets(StdMutex<std::collections::BTreeMap<String, String>>);
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
    fn options() -> ManagedOAuthOptions {
        ManagedOAuthOptions {
            app_namespace: uuid::Uuid::new_v4().to_string(),
            generation: 1,
            connection_id: "connection".into(),
            account_id: "account".into(),
            resource_url: "https://resource.test/mcp".into(),
            redirect_url: "app://callback".into(),
            allowed_origins: vec!["https://resource.test".into()],
            client_id: None,
            client_secret: None,
            client_name: "test".into(),
            scopes: vec![],
        }
    }
    #[tokio::test]
    async fn late_old_vault_write_stays_revoked_and_never_touches_new_generation() {
        let secrets = Arc::new(Secrets::default());
        let options = options();
        let old = BoundStore::new(&options, secrets.clone()).unwrap();
        old.write("credentials", &json!({"secret":"old"}))
            .await
            .unwrap();
        let old_key = format!("{}credentials", old.prefix);
        let late = secrets.0.lock().unwrap()[&old_key].clone();
        let mut next = options.clone();
        next.generation = 2;
        let current = BoundStore::new(&next, secrets.clone()).unwrap();
        current
            .write("credentials", &json!({"secret":"new"}))
            .await
            .unwrap();
        old.revoked.store(true, Ordering::Release);
        old.persist_revocation().await.unwrap();
        old.delete("credentials").await.unwrap();
        drop(old);
        // A non-cancellable vault backend may finish an abandoned write late.
        secrets.0.lock().unwrap().insert(old_key, late);
        let restarted = BoundStore::new(&options, secrets).unwrap();
        assert!(restarted.read::<Value>("credentials").await.is_err());
        assert_eq!(
            current.read::<Value>("credentials").await.unwrap(),
            Some(json!({"secret":"new"}))
        );
    }
    #[tokio::test]
    async fn expired_pkce_state_cannot_be_loaded() {
        let store = BoundStore::new(&options(), Arc::new(Secrets::default())).unwrap();
        let state: StoredAuthorizationState = serde_json::from_value(
            json!({"pkce_verifier":"verifier","csrf_token":"state", "created_at":0}),
        )
        .unwrap();
        <BoundStore as StateStore>::save(&store, "state", state)
            .await
            .unwrap();
        assert!(
            <BoundStore as StateStore>::load(&store, "state")
                .await
                .unwrap()
                .is_none()
        );
    }
}
