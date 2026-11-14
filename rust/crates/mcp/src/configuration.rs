use crate::repository::{McpConfigRepository, RepositoryFault};
use agentkit_contracts::{CreateMcpServerRequest, McpServerDto, UpdateMcpServerRequest};
use agentkit_engine::{NeverFault, SharedRuntime};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::Arc;

mod secrets;
mod state;
use state::{Ports, State};

#[derive(Debug, thiserror::Error)]
pub enum McpConfigError<E: RepositoryFault = NeverFault> {
    #[error("MCP configuration not found")]
    NotFound,
    #[error("MCP alias already exists")]
    DuplicateAlias,
    #[error("invalid MCP configuration")]
    Invalid,
    #[error("MCP configuration changed concurrently; retry")]
    Conflict,
    #[error("MCP credential storage unavailable")]
    SecretStore,
    #[error("MCP live configuration unavailable")]
    Runtime,
    /// The adapter's own fault, branchable only through [`RepositoryFault`]
    /// (ADR 0024 §3); `NeverFault` keeps it unconstructable for the ports
    /// that carry no repository.
    #[error("{0}")]
    Store(#[source] E),
}

impl<E: RepositoryFault> From<E> for McpConfigError<E> {
    fn from(error: E) -> Self {
        Self::Store(error)
    }
}

/// A refusal that names no repository, carried into the fault type of the
/// port its caller drives. The fault-carrying variant is uninhabited on the
/// source side, so the lift loses nothing. (An inherent method, not `From`:
/// a blanket `From` would make every `?` in the service ambiguous.)
impl<E: RepositoryFault> McpConfigError<E> {
    pub fn from_faultless(error: McpConfigError<NeverFault>) -> Self {
        match error {
            McpConfigError::NotFound => Self::NotFound,
            McpConfigError::DuplicateAlias => Self::DuplicateAlias,
            McpConfigError::Invalid => Self::Invalid,
            McpConfigError::Conflict => Self::Conflict,
            McpConfigError::SecretStore => Self::SecretStore,
            McpConfigError::Runtime => Self::Runtime,
            McpConfigError::Store(fault) => match fault {},
        }
    }
}

/// The host vault owns credential bytes; references alone enter SQLite.
#[async_trait]
pub trait McpConfigSecrets: Send + Sync {
    async fn get(&self, reference: &str) -> Result<Option<String>, McpConfigError>;
    async fn set(&self, reference: &str, value: &str) -> Result<(), McpConfigError>;
    async fn delete(&self, reference: &str) -> Result<(), McpConfigError>;
}

/// A cancellation-safe reservation. Dropping it releases only unpublished reservations.
pub trait McpConfigLease: Send {}
impl McpConfigLease for () {}

/// Applies declarations only, without connecting or dispatching remote effects.
#[async_trait]
pub trait McpConfigRuntime: Send + Sync {
    /// Validate and reserve aliases before durable or live authority changes.
    async fn prepare(
        &self,
        _generation: u64,
        _declarations: Vec<McpServerDto>,
    ) -> Result<Box<dyn McpConfigLease>, McpConfigError> {
        Ok(Box::new(()))
    }
    /// Deny affected handles and pending connects synchronously, including repeated fences.
    fn invalidate(&self, generation: u64, aliases: &[String]);
    /// Revoke replaced/deleted sessions before returning. Generations never regress.
    async fn reconcile(
        &self,
        generation: u64,
        declarations: Vec<McpServerDto>,
    ) -> Result<(), McpConfigError>;
    fn metadata(&self, _generation: u64, _alias: &str) -> Option<Value> {
        None
    }
}

/// Stores declarations and references, and returns redacted projections.
/// Generic over its repository port (ADR 0024 §5); composition names the
/// concrete store.
#[derive(Clone)]
pub struct McpConfigurationService<R: McpConfigRepository> {
    store: R,
    app_id: String,
    runtime: SharedRuntime,
    state: Arc<State>,
}

impl<R: McpConfigRepository> McpConfigurationService<R> {
    pub fn new(
        store: R,
        app_id: &str,
        runtime: SharedRuntime,
    ) -> Result<Self, McpConfigError<R::Error>> {
        if app_id.is_empty() {
            return Err(McpConfigError::Invalid);
        }
        let state = state::shared(&store, app_id);
        Ok(Self {
            store,
            app_id: app_id.into(),
            runtime,
            state,
        })
    }

    /// The application scope this repository serves.
    pub fn app_id(&self) -> &str {
        &self.app_id
    }

    /// A live runtime is bound to exactly one store/application scope.
    pub fn with_ports(
        self,
        secrets: Option<Arc<dyn McpConfigSecrets>>,
        live: Option<Arc<dyn McpConfigRuntime>>,
    ) -> Result<Self, McpConfigError<R::Error>> {
        self.state
            .install(&self.store, &self.app_id, secrets, live)?;
        Ok(self)
    }

    /// Run after installing vault/runtime ports and before exposing management routes.
    pub async fn initialize(&self) -> Result<(), McpConfigError<R::Error>> {
        let _guard = self.state.mutation.lock().await;
        let (previous, records) = self.read().await?;
        let ports = self.state.ports();
        let mut next = Vec::new();
        let mut staged = Vec::new();
        for record in records {
            let value = serde_json::to_value(record).map_err(|_| McpConfigError::Invalid)?;
            let (value, writes) = match secrets::normalize(&self.app_id, value, None, &ports).await
            {
                Ok(normalized) => normalized,
                Err(error) => {
                    secrets::cleanup(&staged, &ports).await;
                    return Err(error);
                }
            };
            staged.extend(writes);
            next.push(parse_record(value)?);
        }
        self.apply(previous, next, staged, &ports).await.map(|_| ())
    }

    pub async fn list(&self) -> Result<Vec<McpServerDto>, McpConfigError<R::Error>> {
        let _guard = self.state.mutation.lock().await;
        let (_, records) = self.read().await?;
        let generation = self.applied_generation().await?;
        records
            .into_iter()
            .map(|record| self.project(record, generation))
            .collect()
    }

    pub async fn create(
        &self,
        input: CreateMcpServerRequest,
    ) -> Result<McpServerDto, McpConfigError<R::Error>> {
        let _guard = self.state.mutation.lock().await;
        let (previous, mut records) = self.read().await?;
        let mut value = serde_json::to_value(input).map_err(|_| McpConfigError::Invalid)?;
        let timestamp = self.timestamp()?;
        value["id"] = format!("mcp_{}", self.runtime.id()).into();
        value["createdAt"] = timestamp.clone().into();
        value["updatedAt"] = timestamp.into();
        let checked = parse_record(value.clone())?;
        unique(&records, &checked, None)?;
        let ports = self.state.ports();
        let (value, staged) = secrets::normalize(&self.app_id, value, None, &ports).await?;
        let record = parse_record(value)?;
        records.push(record.clone());
        let generation = self.apply(previous, records, staged, &ports).await?;
        self.project(record, generation)
    }

    pub async fn update(
        &self,
        id: &str,
        input: UpdateMcpServerRequest,
    ) -> Result<McpServerDto, McpConfigError<R::Error>> {
        let _guard = self.state.mutation.lock().await;
        let (previous, mut records) = self.read().await?;
        let index = records
            .iter()
            .position(|record| record.id == id)
            .ok_or(McpConfigError::NotFound)?;
        let old = serde_json::to_value(&records[index]).map_err(|_| McpConfigError::Invalid)?;
        let mut value = old.clone();
        let patch = serde_json::to_value(input).map_err(|_| McpConfigError::Invalid)?;
        for (key, changed) in patch.as_object().ok_or(McpConfigError::Invalid)? {
            if matches!(
                key.as_str(),
                "alias" | "transport" | "secretRefs" | "enabled" | "toolAliases" | "resilience"
            ) {
                value[key] = changed.clone();
            }
        }
        value["updatedAt"] = self.timestamp()?.into();
        unique(&records, &parse_record(value.clone())?, Some(id))?;
        let ports = self.state.ports();
        let (value, staged) = secrets::normalize(&self.app_id, value, Some(&old), &ports).await?;
        let record = parse_record(value)?;
        records[index] = record.clone();
        let generation = self.apply(previous, records, staged, &ports).await?;
        self.project(record, generation)
    }

    pub async fn delete(&self, id: &str) -> Result<(), McpConfigError<R::Error>> {
        let _guard = self.state.mutation.lock().await;
        let (previous, mut records) = self.read().await?;
        let index = records
            .iter()
            .position(|record| record.id == id)
            .ok_or(McpConfigError::NotFound)?;
        records.remove(index);
        let ports = self.state.ports();
        self.apply(previous, records, Vec::new(), &ports).await?;
        Ok(())
    }

    async fn read(&self) -> Result<(Option<Value>, Vec<McpServerDto>), McpConfigError<R::Error>> {
        let value = self.store.get_setting("mcp-config", &self.app_id).await?;
        let mut records = value
            .as_ref()
            .map(|value| {
                value
                    .as_array()
                    .ok_or(McpConfigError::Invalid)?
                    .iter()
                    .cloned()
                    .map(parse_record)
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?
            .unwrap_or_default();
        records.sort_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then(left.id.cmp(&right.id))
        });
        Ok((value, records))
    }

    fn project(
        &self,
        record: McpServerDto,
        generation: u64,
    ) -> Result<McpServerDto, McpConfigError<R::Error>> {
        let mut value = serde_json::to_value(record).map_err(|_| McpConfigError::Invalid)?;
        secrets::redact(&mut value);
        value["generation"] = json!(generation);
        if let Some(live) = self.state.ports().live
            && let Some(metadata) =
                live.metadata(generation, value["alias"].as_str().unwrap_or_default())
            && metadata["generation"].as_u64() == Some(generation)
            && let Some(status) = metadata["status"].as_str().filter(|status| {
                matches!(
                    *status,
                    "connected" | "connecting" | "disconnected" | "failed" | "needs_authentication"
                )
            })
        {
            value["connection"] = json!({"generation":generation,"status":status,"toolCount":metadata["toolCount"].as_u64().unwrap_or(0)});
        }
        serde_json::from_value(value).map_err(|_| McpConfigError::Invalid)
    }

    fn timestamp(&self) -> Result<String, McpConfigError<R::Error>> {
        agentkit_kernel::ConversationTimestamp::from_epoch_millis(self.runtime.now_ms())
            .and_then(|value| value.wire_text().map(|text| text.into_owned()))
            .map_err(|_| McpConfigError::Invalid)
    }
}

fn unique<E: RepositoryFault>(
    records: &[McpServerDto],
    record: &McpServerDto,
    replacing: Option<&str>,
) -> Result<(), McpConfigError<E>> {
    if records
        .iter()
        .any(|item| Some(item.id.as_str()) != replacing && item.alias == record.alias)
    {
        return Err(McpConfigError::DuplicateAlias);
    }
    if replacing.is_none() && records.iter().any(|item| item.id == record.id) {
        return Err(McpConfigError::Conflict);
    }
    Ok(())
}

fn parse_record<E: RepositoryFault>(mut value: Value) -> Result<McpServerDto, McpConfigError<E>> {
    let object = value.as_object_mut().ok_or(McpConfigError::Invalid)?;
    object.retain(|key, _| {
        matches!(
            key.as_str(),
            "id" | "alias"
                | "transport"
                | "secretRefs"
                | "enabled"
                | "toolAliases"
                | "resilience"
                | "createdAt"
                | "updatedAt"
        )
    });
    // Opaque DTO extras are not executable declarations and may contain credentials.
    let http = match value["transport"]["kind"].as_str() {
        Some("http") => true,
        Some("stdio") => false,
        _ => return Err(McpConfigError::Invalid),
    };
    value["transport"]
        .as_object_mut()
        .ok_or(McpConfigError::Invalid)?
        .retain(|key, _| {
            if http {
                matches!(key.as_str(), "kind" | "url" | "headers")
            } else {
                matches!(key.as_str(), "kind" | "command" | "args" | "env")
            }
        });
    agentkit_contracts::validate_schema("McpServerDtoSchema", &value)
        .map_err(|_| McpConfigError::Invalid)?;
    let alias = value["alias"].as_str().ok_or(McpConfigError::Invalid)?;
    if !alias.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        || !alias
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(McpConfigError::Invalid);
    }
    serde_json::from_value(value).map_err(|_| McpConfigError::Invalid)
}
