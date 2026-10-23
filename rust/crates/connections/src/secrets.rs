use crate::{AppId, Connection, ConnectionError, ConnectionId};
use async_trait::async_trait;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
use zeroize::Zeroizing;

/// Secret bytes cannot appear through `Debug`, serialization, or display.
pub struct Secret(Zeroizing<String>);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(Zeroizing::new(value.into()))
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct SecretKey {
    app_id: AppId,
    connection_id: ConnectionId,
    generation: u64,
    account: Option<String>,
    purpose: SecretPurpose,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum SecretPurpose {
    ApiKey,
    Headers,
}

impl SecretKey {
    pub(crate) fn for_connection(connection: &Connection) -> Self {
        Self {
            app_id: connection.app_id.clone(),
            connection_id: connection.id.clone(),
            generation: connection.generation,
            account: connection.config.account.clone(),
            purpose: SecretPurpose::ApiKey,
        }
    }

    pub(crate) fn for_headers(mut self) -> Self {
        self.purpose = SecretPurpose::Headers;
        self
    }

    #[cfg(feature = "os-keychain")]
    fn keychain_account(&self) -> String {
        let app = self.app_id.as_str();
        let connection = self.connection_id.as_str();
        let account = self.account.as_deref().unwrap_or("");
        let purpose = match self.purpose {
            SecretPurpose::ApiKey => "api-key",
            SecretPurpose::Headers => "headers",
        };
        format!(
            "{purpose}:{}:{app}{}:{connection}:{}:{}:{account}",
            app.len(),
            connection.len(),
            self.generation,
            account.len()
        )
    }
}

#[async_trait]
pub trait SecretStore: Send + Sync {
    async fn get(&self, key: &SecretKey) -> Result<Secret, ConnectionError>;
    async fn put(&self, key: &SecretKey, secret: Secret) -> Result<(), ConnectionError>;
    async fn delete(&self, key: &SecretKey) -> Result<(), ConnectionError>;
}

#[derive(Default)]
pub struct MemorySecretStore {
    values: Mutex<BTreeMap<SecretKey, Secret>>,
}

#[async_trait]
impl SecretStore for MemorySecretStore {
    async fn get(&self, key: &SecretKey) -> Result<Secret, ConnectionError> {
        let values = self
            .values
            .lock()
            .map_err(|_| ConnectionError::SecretUnavailable)?;
        values
            .get(key)
            .map(|value| Secret::new(value.expose()))
            .ok_or(ConnectionError::SecretMissing)
    }

    async fn put(&self, key: &SecretKey, secret: Secret) -> Result<(), ConnectionError> {
        self.values
            .lock()
            .map_err(|_| ConnectionError::SecretUnavailable)?
            .insert(key.clone(), secret);
        Ok(())
    }

    async fn delete(&self, key: &SecretKey) -> Result<(), ConnectionError> {
        self.values
            .lock()
            .map_err(|_| ConnectionError::SecretUnavailable)?
            .remove(key);
        Ok(())
    }
}

/// Environment bindings are explicit and read-only; they never fall back to another key.
pub struct EnvironmentSecretStore {
    bindings: BTreeMap<SecretKey, String>,
}

impl EnvironmentSecretStore {
    pub fn new(bindings: BTreeMap<SecretKey, String>) -> Self {
        Self { bindings }
    }
}

#[async_trait]
impl SecretStore for EnvironmentSecretStore {
    async fn get(&self, key: &SecretKey) -> Result<Secret, ConnectionError> {
        let name = self
            .bindings
            .get(key)
            .ok_or(ConnectionError::SecretMissing)?;
        std::env::var(name)
            .map(Secret::new)
            .map_err(|_| ConnectionError::SecretMissing)
    }
    async fn put(&self, _key: &SecretKey, _secret: Secret) -> Result<(), ConnectionError> {
        Err(ConnectionError::SecretUnavailable)
    }
    async fn delete(&self, _key: &SecretKey) -> Result<(), ConnectionError> {
        Err(ConnectionError::SecretUnavailable)
    }
}

/// Explicit unavailable adapter: there is no plaintext or environment fallback.
pub struct UnavailableSecretStore;

#[async_trait]
impl SecretStore for UnavailableSecretStore {
    async fn get(&self, _key: &SecretKey) -> Result<Secret, ConnectionError> {
        Err(ConnectionError::SecretUnavailable)
    }
    async fn put(&self, _key: &SecretKey, _secret: Secret) -> Result<(), ConnectionError> {
        Err(ConnectionError::SecretUnavailable)
    }
    async fn delete(&self, _key: &SecretKey) -> Result<(), ConnectionError> {
        Err(ConnectionError::SecretUnavailable)
    }
}

/// Uses the platform credential service only. Blocking calls run on Tokio's blocking pool.
#[cfg(feature = "os-keychain")]
pub struct KeychainSecretStore {
    service: String,
}

#[cfg(feature = "os-keychain")]
impl KeychainSecretStore {
    pub fn new(service: impl Into<String>) -> Result<Self, ConnectionError> {
        let service = service.into();
        if service.is_empty() {
            return Err(ConnectionError::InvalidIdentifier);
        }
        Ok(Self { service })
    }

    async fn blocking<T: Send + 'static>(
        &self,
        key: &SecretKey,
        operation: impl FnOnce(keyring::Entry) -> Result<T, ConnectionError> + Send + 'static,
    ) -> Result<T, ConnectionError> {
        let service = self.service.clone();
        let account = key.keychain_account();
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| ConnectionError::SecretUnavailable)?;
        runtime
            .spawn_blocking(move || {
                let entry = keyring::Entry::new(&service, &account)
                    .map_err(|_| ConnectionError::SecretUnavailable)?;
                operation(entry)
            })
            .await
            .map_err(|_| ConnectionError::SecretUnavailable)?
    }
}

#[cfg(feature = "os-keychain")]
#[async_trait]
impl SecretStore for KeychainSecretStore {
    async fn get(&self, key: &SecretKey) -> Result<Secret, ConnectionError> {
        self.blocking(key, |entry| match entry.get_password() {
            Ok(value) => Ok(Secret::new(value)),
            Err(keyring::Error::NoEntry) => Err(ConnectionError::SecretMissing),
            Err(_) => Err(ConnectionError::SecretUnavailable),
        })
        .await
    }
    async fn put(&self, key: &SecretKey, secret: Secret) -> Result<(), ConnectionError> {
        self.blocking(key, move |entry| {
            entry
                .set_password(secret.expose())
                .map_err(|_| ConnectionError::SecretUnavailable)
        })
        .await
    }
    async fn delete(&self, key: &SecretKey) -> Result<(), ConnectionError> {
        self.blocking(key, |entry| match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(_) => Err(ConnectionError::SecretUnavailable),
        })
        .await
    }
}

// Keep a convenient trait object alias without exposing concrete credential types.
pub type SharedSecretStore = Arc<dyn SecretStore>;
