use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use thiserror::Error;
use url::Url;

macro_rules! identifier {
    ($name:ident) => {
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);
        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, ConnectionError> {
                let value = value.into();
                if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
                    return Err(ConnectionError::InvalidIdentifier);
                }
                Ok(Self(value))
            }
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
        impl TryFrom<String> for $name {
            type Error = ConnectionError;
            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }
        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }
    };
}
identifier!(AppId);
identifier!(ConnectionId);

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    Chat,
    Responses,
    Embeddings,
    ModelDiscovery,
    Probe,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Model {
    pub id: String,
    pub display_name: Option<String>,
    pub capabilities: BTreeSet<Capability>,
    pub context_window_tokens: Option<u64>,
}

/// Header names are metadata; their values live only in the secret store.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionConfig {
    pub label: String,
    pub provider_kind: String,
    pub endpoint: Url,
    pub account: Option<String>,
    pub credential_required: bool,
    #[serde(default)]
    pub extra_header_names: BTreeSet<String>,
    #[serde(default)]
    pub allow_insecure_http: bool,
    pub enabled: bool,
    pub capabilities: BTreeSet<Capability>,
    pub models: Vec<Model>,
}

impl ConnectionConfig {
    pub(crate) fn validate(&self) -> Result<(), ConnectionError> {
        let local = matches!(
            self.endpoint.host_str(),
            Some("localhost" | "127.0.0.1" | "[::1]")
        );
        let secure = self.endpoint.scheme() == "https"
            || (self.endpoint.scheme() == "http" && (local || self.allow_insecure_http));
        if !secure
            || self.endpoint.host().is_none()
            || !self.endpoint.username().is_empty()
            || self.endpoint.password().is_some()
            || self.endpoint.query().is_some()
            || self.endpoint.fragment().is_some()
            || self.provider_kind.is_empty()
            || self.account.as_ref().is_some_and(|value| value.is_empty())
        {
            return Err(ConnectionError::InvalidConfiguration);
        }
        let ids: BTreeSet<_> = self.models.iter().map(|model| &model.id).collect();
        if ids.len() != self.models.len() || ids.iter().any(|id| id.is_empty()) {
            return Err(ConnectionError::InvalidConfiguration);
        }
        validate_header_names(&self.extra_header_names)?;
        Ok(())
    }
}

pub(crate) fn validate_header_names(names: &BTreeSet<String>) -> Result<(), ConnectionError> {
    let mut normalized = BTreeSet::new();
    for name in names {
        let lower = name.to_ascii_lowercase();
        let reserved = matches!(
            lower.as_str(),
            "authorization"
                | "proxy-authorization"
                | "host"
                | "connection"
                | "content-length"
                | "transfer-encoding"
                | "content-type"
                | "accept"
        );
        let valid = !name.is_empty()
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte));
        if reserved || !valid || !normalized.insert(lower) {
            return Err(ConnectionError::InvalidConfiguration);
        }
    }
    if names.len() > 64 {
        return Err(ConnectionError::InvalidConfiguration);
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Connection {
    pub app_id: AppId,
    pub id: ConnectionId,
    pub generation: u64,
    pub config: ConnectionConfig,
}

/// Canonical native provider projection, deliberately excluding all secrets.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderProjection {
    pub id: String,
    pub label: String,
    pub kind: String,
    pub base_url: String,
    pub default_model: String,
    pub enabled: bool,
    pub app_id: AppId,
    pub connection_id: ConnectionId,
    pub generation: u64,
    pub account: Option<String>,
    pub capabilities: BTreeSet<Capability>,
}

impl Connection {
    pub fn provider(&self, model: &str) -> Result<ProviderProjection, ConnectionError> {
        if !self
            .config
            .models
            .iter()
            .any(|candidate| candidate.id == model)
        {
            return Err(ConnectionError::UnsupportedModel);
        }
        Ok(ProviderProjection {
            id: self.id.as_str().to_owned(),
            label: self.config.label.clone(),
            kind: self.config.provider_kind.clone(),
            base_url: self.config.endpoint.to_string(),
            default_model: model.to_owned(),
            enabled: self.config.enabled,
            app_id: self.app_id.clone(),
            connection_id: self.id.clone(),
            generation: self.generation,
            account: self.config.account.clone(),
            capabilities: self.config.capabilities.clone(),
        })
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ConnectionError {
    #[error("invalid identifier")]
    InvalidIdentifier,
    #[error("invalid connection configuration")]
    InvalidConfiguration,
    #[error("connection already exists")]
    AlreadyExists,
    #[error("connection not found in this app")]
    NotFound,
    #[error("connection generation is stale")]
    StaleGeneration,
    #[error("connection is disabled")]
    Disabled,
    #[error("capability is unavailable")]
    UnsupportedCapability,
    #[error("model is unavailable; explicit selection required")]
    UnsupportedModel,
    #[error("consent is missing, revoked, or bound to another plan")]
    ConsentDenied,
    #[error("secret adapter unavailable")]
    SecretUnavailable,
    #[error("credential not found")]
    SecretMissing,
    #[error("network adapter unavailable")]
    NetworkUnavailable,
    #[error("network request failed")]
    NetworkFailed,
    #[error("repository lock poisoned")]
    Poisoned,
    #[error("connection mutation is pending")]
    MutationPending,
    #[error("connection authority requires reopening after an uncertain commit")]
    AuthorityPoisoned,
    #[error("generation or request identifier exhausted")]
    Exhausted,
}
