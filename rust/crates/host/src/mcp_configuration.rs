use crate::{SharedRuntime, SqliteStore};
use agentkit_contracts::{CreateMcpServerRequest, McpServerDto, UpdateMcpServerRequest};
use serde_json::Value;

#[derive(Debug, thiserror::Error)]
pub enum McpConfigError {
    #[error("MCP configuration not found")]
    NotFound,
    #[error("MCP alias already exists")]
    DuplicateAlias,
    #[error("invalid MCP configuration")]
    Invalid,
    #[error("MCP configuration changed concurrently; retry")]
    Conflict,
    #[error(transparent)]
    Store(#[from] agentkit_store::StoreError),
}

/// Stores declarations and secret references, never resolved credentials.
#[derive(Clone)]
pub struct McpConfigRepository {
    store: SqliteStore,
    app_id: String,
    runtime: SharedRuntime,
}

impl McpConfigRepository {
    pub fn new(
        store: SqliteStore,
        app_id: &str,
        runtime: SharedRuntime,
    ) -> Result<Self, McpConfigError> {
        if app_id.is_empty() {
            return Err(McpConfigError::Invalid);
        }
        Ok(Self {
            store,
            app_id: app_id.into(),
            runtime,
        })
    }

    pub async fn list(&self) -> Result<Vec<McpServerDto>, McpConfigError> {
        Ok(self.read().await?.1)
    }

    pub async fn create(
        &self,
        input: CreateMcpServerRequest,
    ) -> Result<McpServerDto, McpConfigError> {
        let mut value = serde_json::to_value(input).map_err(|_| McpConfigError::Invalid)?;
        let timestamp = self.timestamp()?;
        value["id"] = format!("mcp_{}", self.runtime.id()).into();
        value["createdAt"] = timestamp.clone().into();
        value["updatedAt"] = timestamp.into();
        let record = parse_record(value)?;
        for _ in 0..16 {
            let (previous, mut records) = self.read().await?;
            if records.iter().any(|item| item.alias == record.alias) {
                return Err(McpConfigError::DuplicateAlias);
            }
            if records.iter().any(|item| item.id == record.id) {
                return Err(McpConfigError::Conflict);
            }
            records.push(record.clone());
            if self.commit(previous, records).await? {
                return Ok(record);
            }
        }
        Err(McpConfigError::Conflict)
    }

    pub async fn update(
        &self,
        id: &str,
        input: UpdateMcpServerRequest,
    ) -> Result<McpServerDto, McpConfigError> {
        let patch = serde_json::to_value(input).map_err(|_| McpConfigError::Invalid)?;
        for _ in 0..16 {
            let (previous, mut records) = self.read().await?;
            let index = records
                .iter()
                .position(|item| item.id == id)
                .ok_or(McpConfigError::NotFound)?;
            let mut value =
                serde_json::to_value(&records[index]).map_err(|_| McpConfigError::Invalid)?;
            for (key, value_patch) in patch.as_object().ok_or(McpConfigError::Invalid)? {
                if matches!(
                    key.as_str(),
                    "alias" | "transport" | "secretRefs" | "enabled" | "toolAliases" | "resilience"
                ) {
                    value[key] = value_patch.clone();
                }
            }
            value["updatedAt"] = self.timestamp()?.into();
            let record = parse_record(value)?;
            if records
                .iter()
                .any(|item| item.id != id && item.alias == record.alias)
            {
                return Err(McpConfigError::DuplicateAlias);
            }
            records[index] = record.clone();
            if self.commit(previous, records).await? {
                return Ok(record);
            }
        }
        Err(McpConfigError::Conflict)
    }

    pub async fn delete(&self, id: &str) -> Result<(), McpConfigError> {
        for _ in 0..16 {
            let (previous, mut records) = self.read().await?;
            let index = records
                .iter()
                .position(|item| item.id == id)
                .ok_or(McpConfigError::NotFound)?;
            records.remove(index);
            if self.commit(previous, records).await? {
                return Ok(());
            }
        }
        Err(McpConfigError::Conflict)
    }

    async fn read(&self) -> Result<(Option<Value>, Vec<McpServerDto>), McpConfigError> {
        let value = self.store.get_setting("mcp-config", &self.app_id).await?;
        let mut records: Vec<McpServerDto> = value
            .clone()
            .map(serde_json::from_value)
            .transpose()
            .map_err(|_| McpConfigError::Invalid)?
            .unwrap_or_default();
        records.sort_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then(left.id.cmp(&right.id))
        });
        Ok((value, records))
    }

    async fn commit(
        &self,
        previous: Option<Value>,
        records: Vec<McpServerDto>,
    ) -> Result<bool, McpConfigError> {
        let value = serde_json::to_value(records).map_err(|_| McpConfigError::Invalid)?;
        Ok(self
            .store
            .compare_and_set_setting("mcp-config", &self.app_id, previous, value)
            .await?)
    }

    fn timestamp(&self) -> Result<String, McpConfigError> {
        agentkit_store::ConversationTimestamp::from_epoch_millis(self.runtime.now_ms())
            .and_then(|value| value.wire_text().map(|text| text.into_owned()))
            .map_err(|_| McpConfigError::Invalid)
    }
}

fn parse_record(mut value: Value) -> Result<McpServerDto, McpConfigError> {
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

#[cfg(feature = "http")]
#[path = "mcp_configuration_http.rs"]
mod http_adapter;
