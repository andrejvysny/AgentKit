use crate::{ClientOptions, McpClientError};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, time::Duration};

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum TransportConfig {
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
    },
    Http {
        url: String,
        #[serde(default)]
        headers: BTreeMap<String, String>,
    },
}

/// Config contains declarations and references; resolved values are never stored here.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerConfig {
    pub alias: String,
    pub transport: TransportConfig,
    #[serde(default)]
    pub secret_refs: BTreeMap<String, String>,
    #[serde(default = "enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub tool_aliases: BTreeMap<String, String>,
    #[serde(default)]
    pub resilience: Resilience,
}
fn enabled() -> bool {
    true
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Resilience {
    pub request_timeout_ms: u64,
    pub connect_timeout_ms: u64,
    pub max_connect_attempts: u32,
    pub connect_backoff_base_ms: u64,
    pub connect_backoff_max_ms: u64,
    pub circuit_open_ms: u64,
    pub reconnect_max_attempts: u32,
    pub reconnect_backoff_factor: f64,
    pub retry_timeouts: bool,
}
impl Default for Resilience {
    fn default() -> Self {
        Self {
            request_timeout_ms: 5000,
            connect_timeout_ms: 5000,
            max_connect_attempts: 3,
            connect_backoff_base_ms: 250,
            connect_backoff_max_ms: 2000,
            circuit_open_ms: 5000,
            reconnect_max_attempts: 2,
            reconnect_backoff_factor: 2.0,
            retry_timeouts: false,
        }
    }
}
impl Resilience {
    pub(super) fn client_options(&self) -> ClientOptions {
        ClientOptions {
            connect_timeout: Duration::from_millis(self.connect_timeout_ms),
            request_timeout: Duration::from_millis(self.request_timeout_ms),
            ..ClientOptions::default()
        }
    }
    pub(super) fn backoff(&self, attempt: u32, factor: f64) -> Duration {
        let value = self.connect_backoff_base_ms as f64 * factor.powf(f64::from(attempt) - 1.0);
        Duration::from_millis(value.round().min(self.connect_backoff_max_ms as f64) as u64)
    }
    fn validate(&self) -> bool {
        self.request_timeout_ms > 0
            && self.connect_timeout_ms > 0
            && self.max_connect_attempts > 0
            && self.reconnect_backoff_factor.is_finite()
            && self.reconnect_backoff_factor >= 1.0
    }
}
impl ServerConfig {
    pub(super) fn normalize(mut self) -> Result<Self, McpClientError> {
        self.alias = self.alias.trim().into();
        if !crate::bridge::server_alias(&self.alias) || !self.resilience.validate() {
            return Err(McpClientError::Configuration);
        }
        let mut targets = std::collections::BTreeSet::new();
        let mut aliases = BTreeMap::new();
        for (source, target) in self.tool_aliases {
            let (source, target) = (source.trim().to_owned(), target.trim().to_owned());
            if !crate::bridge::tool_name(&source)
                || !crate::bridge::tool_name(&target)
                || !targets.insert(target.clone())
                || aliases.insert(source, target).is_some()
            {
                return Err(McpClientError::Configuration);
            }
        }
        self.tool_aliases = aliases;
        Ok(self)
    }
}
