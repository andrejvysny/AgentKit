use super::{
    McpConfigError, McpConfigRepository, McpConfigRuntime, McpConfigSecrets, McpServerDto,
    SqliteStore, parse_record, secrets,
};
use agentkit_store::{SettingWrite, SettingsCommit, StoreActorIdentity};
use serde_json::{Value, json};
use std::sync::{Arc, LazyLock, Mutex, Weak};

#[derive(Clone, Default)]
pub(super) struct Ports {
    pub secrets: Option<Arc<dyn McpConfigSecrets>>,
    pub live: Option<Arc<dyn McpConfigRuntime>>,
}

#[derive(Default)]
pub(super) struct State {
    pub mutation: tokio::sync::Mutex<()>,
    ports: Mutex<Ports>,
}

impl State {
    pub fn ports(&self) -> Ports {
        self.ports.lock().expect("MCP configuration ports").clone()
    }
    pub fn install(
        &self,
        store: &SqliteStore,
        app_id: &str,
        secrets: Option<Arc<dyn McpConfigSecrets>>,
        live: Option<Arc<dyn McpConfigRuntime>>,
    ) -> Result<(), McpConfigError> {
        let mut ports = self.ports.lock().expect("MCP configuration ports");
        if matches!((&ports.secrets, &secrets), (Some(old), Some(next)) if !Arc::ptr_eq(old, next))
            || matches!((&ports.live, &live), (Some(old), Some(next)) if !Arc::ptr_eq(old, next))
        {
            return Err(McpConfigError::Conflict);
        }
        if let Some(runtime) = &live {
            bind_runtime(store, app_id, runtime)?;
        }
        if secrets.is_some() {
            ports.secrets = secrets;
        }
        if live.is_some() {
            ports.live = live;
        }
        Ok(())
    }
}

struct RuntimeBinding {
    identity: StoreActorIdentity,
    app_id: String,
    runtime: Weak<dyn McpConfigRuntime>,
}
static RUNTIMES: LazyLock<Mutex<Vec<RuntimeBinding>>> = LazyLock::new(Default::default);

fn bind_runtime(
    store: &SqliteStore,
    app_id: &str,
    runtime: &Arc<dyn McpConfigRuntime>,
) -> Result<(), McpConfigError> {
    let mut bindings = RUNTIMES.lock().expect("MCP runtime bindings");
    bindings.retain(|entry| entry.runtime.strong_count() > 0);
    let runtime = Arc::downgrade(runtime);
    if let Some(entry) = bindings.iter().find(|entry| entry.runtime.ptr_eq(&runtime)) {
        return if entry.app_id == app_id && entry.identity.matches(store) {
            Ok(())
        } else {
            Err(McpConfigError::Conflict)
        };
    }
    bindings.push(RuntimeBinding {
        identity: store.actor_identity(),
        app_id: app_id.into(),
        runtime,
    });
    Ok(())
}

type Entry = (StoreActorIdentity, String, Weak<State>);
static STATES: LazyLock<Mutex<Vec<Entry>>> = LazyLock::new(Default::default);

pub(super) fn shared(store: &SqliteStore, app_id: &str) -> Arc<State> {
    let mut states = STATES.lock().expect("MCP configuration scopes");
    states.retain(|(identity, _, state)| identity.is_alive() && state.strong_count() > 0);
    if let Some(state) = states
        .iter()
        .find(|(identity, app, _)| app == app_id && identity.matches(store))
        .and_then(|(_, _, state)| state.upgrade())
    {
        return state;
    }
    let state = Arc::new(State::default());
    states.push((
        store.actor_identity(),
        app_id.into(),
        Arc::downgrade(&state),
    ));
    state
}

impl McpConfigRepository {
    pub(super) async fn applied_generation(&self) -> Result<u64, McpConfigError> {
        let value = self
            .store
            .get_setting("mcp-config-generation", &self.app_id)
            .await?;
        value
            .map(|value| {
                value
                    .as_u64()
                    .filter(|value| *value <= 9_007_199_254_740_991)
                    .ok_or(McpConfigError::Invalid)
            })
            .unwrap_or(Ok(0))
    }

    async fn reserve_generation(&self) -> Result<u64, McpConfigError> {
        for _ in 0..16 {
            let previous = self
                .store
                .get_setting("mcp-config-counter", &self.app_id)
                .await?;
            let current = previous
                .as_ref()
                .map(|value| value.as_u64().ok_or(McpConfigError::Invalid))
                .transpose()?
                .unwrap_or(0);
            if current < self.applied_generation().await? {
                return Err(McpConfigError::Invalid);
            }
            let next = current
                .checked_add(1)
                .filter(|value| *value <= 9_007_199_254_740_991)
                .ok_or(McpConfigError::Invalid)?;
            if self
                .store
                .compare_and_set_setting("mcp-config-counter", &self.app_id, previous, json!(next))
                .await?
            {
                return Ok(next);
            }
        }
        Err(McpConfigError::Conflict)
    }

    pub(super) async fn apply(
        &self,
        previous: Option<Value>,
        records: Vec<McpServerDto>,
        staged: Vec<String>,
        ports: &Ports,
    ) -> Result<u64, McpConfigError> {
        if let Err(error) = secrets::validate_declarations(&records) {
            secrets::cleanup(&staged, ports).await;
            return Err(error);
        }
        let generation = match self.reserve_generation().await {
            Ok(generation) => generation,
            Err(error) => {
                secrets::cleanup(&staged, ports).await;
                return Err(error);
            }
        };
        let _lease = match prepare(generation, &records, ports).await {
            Ok(lease) => lease,
            Err(error) => {
                secrets::cleanup(&staged, ports).await;
                return Err(error);
            }
        };
        let old = previous
            .as_ref()
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if let Some(live) = &ports.live {
            live.invalidate(generation, &changed_aliases(&old, &records));
        }
        let result = self.commit_snapshot(previous, &records, generation).await;
        match result {
            Ok(SettingsCommit::Applied) => {
                self.retire_secrets(&old, &records, ports).await;
                self.activate(generation, &old, &records, ports).await?;
                Ok(generation)
            }
            failed => {
                self.fence_failed_runtime(generation, &old, &records, ports)
                    .await;
                failed_commit(failed, &staged, ports).await
            }
        }
    }

    async fn commit_snapshot(
        &self,
        previous: Option<Value>,
        records: &[McpServerDto],
        generation: u64,
    ) -> Result<SettingsCommit, McpConfigError> {
        let value = serde_json::to_value(records).map_err(|_| McpConfigError::Invalid)?;
        Ok(self
            .store
            .compare_and_set_settings(
                "mcp-config",
                &self.app_id,
                previous,
                vec![
                    SettingWrite {
                        namespace: "mcp-config".into(),
                        key: self.app_id.clone(),
                        value: Some(value),
                    },
                    SettingWrite {
                        namespace: "mcp-config-generation".into(),
                        key: self.app_id.clone(),
                        value: Some(json!(generation)),
                    },
                ],
            )
            .await?)
    }

    async fn activate(
        &self,
        generation: u64,
        old: &[Value],
        records: &[McpServerDto],
        ports: &Ports,
    ) -> Result<(), McpConfigError> {
        if let Some(live) = &ports.live
            && !matches!(
                tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    live.reconcile(generation, records.to_vec())
                )
                .await,
                Ok(Ok(()))
            )
        {
            self.fence_failed_runtime(generation, old, records, ports)
                .await;
            return Err(McpConfigError::Runtime);
        }
        Ok(())
    }

    async fn retire_secrets(&self, old: &[Value], next: &[McpServerDto], ports: &Ports) {
        for previous in old {
            let current = next
                .iter()
                .find(|record| previous["id"].as_str() == Some(record.id.as_str()))
                .and_then(|record| serde_json::to_value(record).ok())
                .unwrap_or(Value::Null);
            secrets::retire(&self.app_id, previous, &current, ports).await;
        }
    }

    async fn fence_failed_runtime(
        &self,
        generation: u64,
        old: &[Value],
        next: &[McpServerDto],
        ports: &Ports,
    ) {
        if let Some(live) = &ports.live {
            let mut aliases = old
                .iter()
                .filter_map(|value| value["alias"].as_str().map(str::to_owned))
                .collect::<Vec<_>>();
            aliases.extend(next.iter().map(|record| record.alias.clone()));
            live.invalidate(generation, &aliases);
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                live.reconcile(generation, Vec::new()),
            )
            .await;
        }
    }
}

async fn prepare(
    generation: u64,
    records: &[McpServerDto],
    ports: &Ports,
) -> Result<Option<Box<dyn super::McpConfigLease>>, McpConfigError> {
    match &ports.live {
        Some(live) => tokio::time::timeout(
            std::time::Duration::from_secs(5),
            live.prepare(generation, records.to_vec()),
        )
        .await
        .map_err(|_| McpConfigError::Runtime)?
        .map(Some),
        None => Ok(None),
    }
}

fn changed_aliases(previous: &[Value], next: &[McpServerDto]) -> Vec<String> {
    let previous = previous
        .iter()
        .filter_map(|value| parse_record(value.clone()).ok())
        .collect::<Vec<_>>();
    let unchanged = |left: &McpServerDto, right: &McpServerDto| {
        let mut right = right.clone();
        right.updated_at = left.updated_at.clone();
        *left == right
    };
    let old = previous
        .iter()
        .filter(|record| !next.iter().any(|next| unchanged(record, next)));
    let new = next
        .iter()
        .filter(|record| !previous.iter().any(|old| unchanged(record, old)));
    old.chain(new)
        .map(|record| record.alias.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect()
}

async fn failed_commit(
    failed: Result<SettingsCommit, McpConfigError>,
    staged: &[String],
    ports: &Ports,
) -> Result<u64, McpConfigError> {
    match failed {
        Ok(SettingsCommit::Conflict) => {
            secrets::cleanup(staged, ports).await;
            Err(McpConfigError::Conflict)
        }
        Ok(SettingsCommit::Aborted(error)) => {
            secrets::cleanup(staged, ports).await;
            Err(error.into())
        }
        // An outer failure cannot prove rollback; persisted refs must stay resolvable.
        Err(error) => Err(error),
        Ok(SettingsCommit::Applied) => unreachable!(),
    }
}
