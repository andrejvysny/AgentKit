use super::*;
use crate::jsbridge::{JsBridge, PendingKind};
use agentkit_contracts::McpServerDto;
use agentkit_mcp::{McpConfigError, McpConfigLease, McpConfigRuntime, McpConfigSecrets};
use napi::threadsafe_function::ThreadsafeFunction;
use std::{
    collections::BTreeSet,
    sync::{
        Mutex as StdMutex,
        atomic::{AtomicBool, Ordering},
    },
};

#[cfg(test)]
#[path = "mcp_client_runtime_tests.rs"]
mod tests;

pub(crate) type ConfigPorts = (Arc<dyn McpConfigRuntime>, Arc<dyn McpConfigSecrets>);

#[derive(Default)]
pub(crate) struct RuntimeState {
    pub(super) manager: Option<std::sync::Weak<McpClientManager>>,
    pub(super) generation: u64,
    pub(super) ready: bool,
    pub(super) owned: BTreeSet<String>,
    pub(super) blocked: BTreeSet<String>,
    pub(super) reservation: Option<(u64, BTreeSet<String>)>,
    pub(super) pending: HashMap<String, (String, CancellationToken)>,
    pub(super) tool_counts: HashMap<String, usize>,
    vault: Option<Arc<JsBridge>>,
    vault_generation: u64,
}
impl RuntimeState {
    pub(super) fn manager(&self) -> Option<Arc<McpClientManager>> {
        self.manager.as_ref().and_then(std::sync::Weak::upgrade)
    }
    pub(super) fn take_vault(&mut self) -> Option<Arc<JsBridge>> {
        self.vault.take()
    }
}

type Shared = Arc<StdMutex<RuntimeState>>;
pub(super) struct Vault {
    shared: Shared,
}
struct Runtime {
    shared: Shared,
    registry: Arc<tokio::sync::Mutex<Option<Registry>>>,
    gate: Arc<tokio::sync::Mutex<()>>,
    closed: Arc<AtomicBool>,
}

struct Reservation {
    shared: Shared,
    generation: u64,
}
impl McpConfigLease for Reservation {}
impl Drop for Reservation {
    fn drop(&mut self) {
        let mut shared = self
            .shared
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if shared
            .reservation
            .as_ref()
            .is_some_and(|(generation, _)| *generation == self.generation)
        {
            shared.reservation = None;
        }
    }
}

fn declarations(
    records: Vec<McpServerDto>,
) -> std::result::Result<BTreeMap<String, ServerConfig>, McpConfigError> {
    records
        .into_iter()
        .map(|record| {
            serde_json::to_value(record)
                .ok()
                .and_then(|value| serde_json::from_value::<ServerConfig>(value).ok())
                .map(|config| (config.alias.clone(), config))
                .ok_or(McpConfigError::Invalid)
        })
        .collect()
}

impl Vault {
    pub(super) fn new(shared: Shared) -> Self {
        Self { shared }
    }
    async fn invoke(&self, request: Value) -> std::result::Result<Value, McpConfigError> {
        let bridge = self
            .shared
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .vault
            .clone()
            .ok_or(McpConfigError::SecretStore)?;
        client_oauth::callback(&bridge, PendingKind::McpOAuthSecret, request)
            .await
            .map_err(|_| McpConfigError::SecretStore)
    }
}
#[async_trait]
impl McpConfigSecrets for Vault {
    async fn get(&self, reference: &str) -> std::result::Result<Option<String>, McpConfigError> {
        match self
            .invoke(json!({"operation":"get","key":reference}))
            .await?
        {
            Value::Null => Ok(None),
            Value::String(value) => Ok(Some(value)),
            _ => Err(McpConfigError::SecretStore),
        }
    }
    async fn set(&self, reference: &str, value: &str) -> std::result::Result<(), McpConfigError> {
        self.invoke(json!({"operation":"set","key":reference,"value":value}))
            .await
            .map(|_| ())
    }
    async fn delete(&self, reference: &str) -> std::result::Result<(), McpConfigError> {
        self.invoke(json!({"operation":"delete","key":reference}))
            .await
            .map(|_| ())
    }
}
#[async_trait]
impl McpSecretResolver for Vault {
    async fn get(&self, reference: &str) -> std::result::Result<Option<String>, McpClientError> {
        <Self as McpConfigSecrets>::get(self, reference)
            .await
            .map_err(|_| McpClientError::SecretMissing)
    }
    async fn generation(
        &self,
        _: &BTreeMap<String, String>,
    ) -> std::result::Result<u64, McpClientError> {
        Ok(self
            .shared
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .vault_generation)
    }
}

#[async_trait]
impl McpConfigRuntime for Runtime {
    async fn prepare(
        &self,
        generation: u64,
        records: Vec<McpServerDto>,
    ) -> std::result::Result<Box<dyn McpConfigLease>, McpConfigError> {
        let next = declarations(records)?;
        let guard = self.registry.lock().await;
        let registry = guard.as_ref().ok_or(McpConfigError::Runtime)?;
        let mut shared = self
            .shared
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if self.closed.load(Ordering::Acquire)
            || generation < shared.generation
            || shared.reservation.is_some()
        {
            return Err(McpConfigError::Conflict);
        }
        merge(registry, &shared.owned, &next)?;
        let mut aliases = shared.owned.clone();
        aliases.extend(next.keys().cloned());
        // Changed durable candidates are revoked by invalidate immediately
        // after this lease is returned. Other dials cannot cross the commit.
        if shared.pending.keys().any(|alias| {
            !shared.owned.contains(alias) || registry.configs.get(alias) == next.get(alias)
        }) {
            return Err(McpConfigError::Conflict);
        }
        shared.reservation = Some((generation, aliases));
        Ok(Box::new(Reservation {
            shared: self.shared.clone(),
            generation,
        }))
    }
    fn invalidate(&self, generation: u64, aliases: &[String]) {
        let manager = {
            let mut shared = self
                .shared
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if generation < shared.generation {
                return;
            }
            shared.generation = generation;
            shared.ready = false;
            for alias in aliases {
                shared.blocked.insert(alias.clone());
                if let Some((_, token)) = shared.pending.get(alias) {
                    token.cancel();
                }
            }
            shared.manager()
        };
        if let Some(manager) = manager {
            for alias in aliases {
                manager.revoke_alias(alias);
            }
        }
    }
    async fn reconcile(
        &self,
        generation: u64,
        declarations: Vec<McpServerDto>,
    ) -> std::result::Result<(), McpConfigError> {
        let next = self::declarations(declarations)?;
        let _gate = self.gate.lock().await;
        let mut guard = self.registry.lock().await;
        let registry = guard.as_mut().ok_or(McpConfigError::Runtime)?;
        let (owned, blocked) = {
            let shared = self
                .shared
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if self.closed.load(Ordering::Acquire) || generation != shared.generation {
                return Err(McpConfigError::Conflict);
            }
            (shared.owned.clone(), shared.blocked.clone())
        };
        let target = merge(registry, &owned, &next)?;
        let mut changed = registry
            .configs
            .iter()
            .filter(|(alias, config)| target.get(*alias) != Some(*config))
            .map(|(alias, _)| alias.clone())
            .collect::<BTreeSet<_>>();
        changed.extend(blocked);
        registry
            .manager
            .replace_configs(target.values().cloned().collect())
            .await
            .map_err(|_| McpConfigError::Runtime)?;
        let mut shared = self
            .shared
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if self.closed.load(Ordering::Acquire) || generation != shared.generation {
            for alias in target.keys() {
                registry.manager.revoke_alias(alias);
            }
            return Err(McpConfigError::Conflict);
        }
        for alias in changed {
            registry.tools.remove(&alias);
            shared.tool_counts.remove(&alias);
            close_callback(&registry.callbacks, &alias);
            registry
                .oauth
                .write()
                .unwrap_or_else(|error| error.into_inner())
                .remove(&alias);
        }
        registry.configs = target;
        shared.owned = next.keys().cloned().collect();
        shared.blocked.clear();
        shared.ready = true;
        Ok(())
    }
    fn metadata(&self, generation: u64, alias: &str) -> Option<Value> {
        let shared = self
            .shared
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if self.closed.load(Ordering::Acquire)
            || !shared.ready
            || generation != shared.generation
            || !shared.owned.contains(alias)
        {
            return None;
        }
        let connected = shared.manager().is_some_and(|manager| {
            manager
                .connected_aliases()
                .iter()
                .any(|known| known == alias)
        });
        Some(
            json!({"generation":generation,"status":if connected {"connected"} else if shared.pending.contains_key(alias) {"connecting"} else {"disconnected"},
            "toolCount":if connected {shared.tool_counts.get(alias).copied().unwrap_or(0)} else {0}}),
        )
    }
}

fn merge(
    registry: &Registry,
    owned: &BTreeSet<String>,
    next: &BTreeMap<String, ServerConfig>,
) -> std::result::Result<BTreeMap<String, ServerConfig>, McpConfigError> {
    let mut target = registry
        .configs
        .iter()
        .filter(|(alias, _)| !owned.contains(*alias))
        .map(|(alias, config)| (alias.clone(), config.clone()))
        .collect::<BTreeMap<_, _>>();
    for (alias, config) in next {
        if !owned.contains(alias) && target.get(alias).is_some_and(|current| current != config) {
            return Err(McpConfigError::Conflict);
        }
        target.insert(alias.clone(), config.clone());
    }
    Ok(target)
}

impl NativeStore {
    pub(crate) async fn mcp_config_ports(&self) -> Result<ConfigPorts> {
        let mut state = self.mcp.lock().await;
        if self.mcp_closed.load(Ordering::Acquire) {
            return Err(errors::closed("the store is closing"));
        }
        self.handle()?;
        if state.is_none() {
            *state = Some(client_ports::registry(self.mcp_shared.clone())?);
        }
        Ok(self
            .mcp_ports
            .get_or_init(|| {
                let runtime = Arc::new(Runtime {
                    shared: self.mcp_shared.clone(),
                    registry: self.mcp.clone(),
                    gate: self.mcp_connect_gate.clone(),
                    closed: self.mcp_closed.clone(),
                });
                let vault = Arc::new(Vault::new(self.mcp_shared.clone()));
                (
                    runtime as Arc<dyn McpConfigRuntime>,
                    vault as Arc<dyn McpConfigSecrets>,
                )
            })
            .clone())
    }
}

#[napi]
impl NativeStore {
    #[napi]
    pub async fn mcp_configure_secrets(&self, deliver: ThreadsafeFunction<String>) -> Result<()> {
        self.mcp_config_ports().await?;
        let mut registry = self.mcp.lock().await;
        if self.mcp_closed.load(Ordering::Acquire) {
            return Err(errors::closed("the store is closing"));
        }
        let registry = registry
            .as_mut()
            .ok_or_else(|| errors::closed("the store is closed"))?;
        let (old, manager) = {
            let mut shared = self
                .mcp_shared
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if shared.vault.is_some() {
                return Err(errors::invalid(
                    "MCP secret port already configured; reopen store to replace authority",
                ));
            }
            shared.vault_generation = shared
                .vault_generation
                .checked_add(1)
                .ok_or_else(|| errors::invalid("MCP vault generation exhausted"))?;
            (
                shared.vault.replace(Arc::new(JsBridge::new(deliver))),
                shared.manager(),
            )
        };
        if let Some(old) = old {
            old.start_closing();
            old.mark_closed();
        }
        if let Some(manager) = manager {
            for (alias, config) in &registry.configs {
                if !config.secret_refs.is_empty() {
                    manager.revoke_alias(alias);
                }
            }
        }
        registry.tools.retain(|alias, _| {
            registry
                .configs
                .get(alias)
                .is_some_and(|config| config.secret_refs.is_empty())
        });
        Ok(())
    }
}
