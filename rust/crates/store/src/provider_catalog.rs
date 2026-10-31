//! Native provider catalogue stored as JSON in the existing settings table.

use crate::{Result, SqliteStore, StoreError, util::json};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use std::collections::HashSet;

pub const PROVIDERS_NAMESPACE: &str = "agentkit.compat.providers";
pub const PROVIDER_MODELS_NAMESPACE: &str = "agentkit.compat.provider-models";
pub const PROVIDER_CAPABILITIES_NAMESPACE: &str = "agentkit.compat.provider-capabilities";

fn invalid(kind: &str) -> StoreError {
    StoreError::RecoveryUnreadable(format!("Malformed {kind} record"))
}
fn parse(text: &str, kind: &str) -> Result<Value> {
    agentkit_contracts::parse_javascript_json(text).map_err(|_| invalid(kind))
}
fn required_strings(value: &Value, fields: &[&str]) -> bool {
    value.is_object() && fields.iter().all(|field| value[*field].is_string())
}
fn provider(value: &Value) -> bool {
    required_strings(value, &["id", "label", "kind", "baseUrl", "defaultModel"])
        && value["enabled"].is_boolean()
        && value.get("apiKey").is_none_or(Value::is_string)
        && value.get("metadata").is_none_or(Value::is_object)
        && value.get("extraHeaders").is_none_or(|headers| {
            headers
                .as_object()
                .is_some_and(|items| items.values().all(Value::is_string))
        })
}
fn model(value: &Value) -> bool {
    required_strings(value, &["providerId", "modelId", "fetchedAt"])
        && value
            .get("displayName")
            .is_some_and(|name| name.is_string() || name.is_null())
        && value
            .get("contextWindowTokens")
            .is_none_or(Value::is_number)
        && value
            .get("supportsToolCalling")
            .is_none_or(Value::is_boolean)
}
fn capabilities(value: &Value) -> bool {
    value.is_object()
        && ["streaming", "toolCalling", "modelList"]
            .iter()
            .all(|field| value[*field].is_boolean())
        && ["vision", "audio", "jsonMode"]
            .iter()
            .all(|field| value.get(*field).is_none_or(Value::is_boolean))
        && value.get("maxContextTokens").is_none_or(Value::is_number)
        && ["checkedAt", "warning"]
            .iter()
            .all(|field| value.get(*field).is_none_or(Value::is_string))
}
fn get(db: &Connection, namespace: &str, key: &str, kind: &str) -> Result<Option<Value>> {
    let text: Option<String> = db
        .query_row(
            "SELECT value FROM settings WHERE namespace=? AND key=?",
            params![namespace, key],
            |row| row.get(0),
        )
        .optional()?;
    text.map(|text| parse(&text, kind)).transpose()
}
fn save(db: &Connection, namespace: &str, key: &str, value: &Value) -> Result<()> {
    db.execute("INSERT INTO settings(namespace,key,value) VALUES(?,?,?) ON CONFLICT(namespace,key) DO UPDATE SET value=excluded.value", params![namespace, key, json(db, value)?])?;
    Ok(())
}

impl SqliteStore {
    pub async fn list_providers(&self) -> Result<Vec<Value>> {
        self.call(|db| {
            let mut statement =
                db.prepare("SELECT key,value FROM settings WHERE namespace=? ORDER BY rowid")?;
            let rows = statement.query_map([PROVIDERS_NAMESPACE], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            rows.map(|row| {
                let (key, text) = row?;
                let value = parse(&text, "provider")?;
                if !provider(&value) || value["id"].as_str() != Some(key.as_str()) {
                    return Err(invalid("provider"));
                }
                Ok(value)
            })
            .collect()
        })
        .await
    }
    pub async fn get_provider(&self, id: &str) -> Result<Option<Value>> {
        let id = id.to_owned();
        self.call(move |db| {
            let value = get(db, PROVIDERS_NAMESPACE, &id, "provider")?;
            if value
                .as_ref()
                .is_some_and(|value| !provider(value) || value["id"].as_str() != Some(id.as_str()))
            {
                return Err(invalid("provider"));
            }
            Ok(value)
        })
        .await
    }
    pub async fn upsert_provider(&self, value: Value) -> Result<Value> {
        if !provider(&value) {
            return Err(StoreError::InvalidInput("Malformed provider".into()));
        }
        self.call(move |db| {
            let id = value["id"].as_str().expect("validated provider");
            save(db, PROVIDERS_NAMESPACE, id, &value)?;
            Ok(value)
        })
        .await
    }
    pub async fn delete_provider(&self, id: &str) -> Result<()> {
        let id = id.to_owned();
        self.call(move |db| {
            let tx = crate::transaction::atomic(db)?;
            for namespace in [
                PROVIDERS_NAMESPACE,
                PROVIDER_MODELS_NAMESPACE,
                PROVIDER_CAPABILITIES_NAMESPACE,
            ] {
                tx.execute(
                    "DELETE FROM settings WHERE namespace=? AND key=?",
                    params![namespace, id],
                )?;
            }
            tx.commit()?;
            Ok(())
        })
        .await
    }
    pub async fn list_provider_models(&self, id: &str) -> Result<Vec<Value>> {
        let id = id.to_owned();
        self.call(move |db| {
            let Some(value) = get(db, PROVIDER_MODELS_NAMESPACE, &id, "provider models")? else {
                return Ok(vec![]);
            };
            let mut models = value
                .as_array()
                .ok_or_else(|| invalid("provider models"))?
                .clone();
            let mut seen = HashSet::new();
            if models.iter().any(|item| {
                !model(item)
                    || !seen.insert(item["modelId"].as_str().unwrap_or_default().to_owned())
            }) {
                return Err(invalid("provider models"));
            }
            models.sort_by(|a, b| a["modelId"].as_str().cmp(&b["modelId"].as_str()));
            Ok(models)
        })
        .await
    }
    pub async fn replace_provider_models(&self, id: &str, models: Vec<Value>) -> Result<()> {
        let mut seen = HashSet::new();
        if models.iter().any(|item| {
            !model(item) || !seen.insert(item["modelId"].as_str().unwrap_or_default().to_owned())
        }) {
            return Err(StoreError::InvalidInput(
                "Malformed or duplicate provider model".into(),
            ));
        }
        let id = id.to_owned();
        self.call(move |db| save(db, PROVIDER_MODELS_NAMESPACE, &id, &Value::Array(models)))
            .await
    }
    pub async fn get_provider_capabilities(&self, id: &str) -> Result<Option<Value>> {
        let id = id.to_owned();
        self.call(move |db| {
            let value = get(
                db,
                PROVIDER_CAPABILITIES_NAMESPACE,
                &id,
                "provider capabilities",
            )?;
            if value.as_ref().is_some_and(|value| !capabilities(value)) {
                return Err(invalid("provider capabilities"));
            }
            Ok(value)
        })
        .await
    }
    pub async fn save_provider_capabilities(&self, id: &str, value: Value) -> Result<()> {
        if !capabilities(&value) {
            return Err(StoreError::InvalidInput(
                "Malformed provider capabilities".into(),
            ));
        }
        let id = id.to_owned();
        self.call(move |db| save(db, PROVIDER_CAPABILITIES_NAMESPACE, &id, &value))
            .await
    }
}
