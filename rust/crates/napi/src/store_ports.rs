use super::NativeStore;
use crate::errors;
use agentkit_store::{LeaseFence, SqliteStore};
use napi::bindgen_prelude::*;
use serde_json::{Value, json};

pub(super) fn text<'a>(input: &'a Value, field: &str) -> Result<&'a str> {
    input[field]
        .as_str()
        .ok_or_else(|| errors::invalid(format!("{field} must be a string")))
}

pub(super) fn opt_text<'a>(input: &'a Value, field: &str) -> Result<Option<&'a str>> {
    match input.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        _ => Err(errors::invalid(format!("{field} must be a string"))),
    }
}

pub(super) fn required_i64(input: &Value, field: &str) -> Result<i64> {
    let value = &input[field];
    if let Some(integer) = value.as_i64() {
        return Ok(integer);
    }
    value
        .as_f64()
        .filter(|number| number.is_finite() && number.fract() == 0.0)
        .map(|number| number as i64)
        .ok_or_else(|| errors::invalid(format!("{field} must be an integer")))
}

pub(super) fn opt_i64(input: &Value, field: &str) -> Result<Option<i64>> {
    match input.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(_) => required_i64(input, field).map(Some),
    }
}

/// A JavaScript `Date` crossing the JSON boundary: epoch milliseconds or an
/// ISO-8601 instant (the shape `JSON.stringify(new Date(...))` produces).
pub(super) fn instant(input: &Value, field: &str) -> Result<i64> {
    match &input[field] {
        Value::Number(_) => required_i64(input, field),
        Value::String(value) => value
            .parse::<i64>()
            .ok()
            .or_else(|| agentkit_store::parse_answer_expiry(value))
            .ok_or_else(|| errors::invalid(format!("{field} must be an instant"))),
        _ => Err(errors::invalid(format!(
            "{field} must be an epoch-millis number or ISO instant"
        ))),
    }
}

pub(super) fn opt_instant(input: &Value, field: &str) -> Result<Option<i64>> {
    match input.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(_) => instant(input, field).map(Some),
    }
}

/// Ports that take no `Date` parameter let the store's clock stamp the write;
/// callers may pass `now` (epoch millis or ISO) for deterministic tests.
pub(super) fn instant_or_now(input: &Value, field: &str) -> Result<i64> {
    opt_instant(input, field).map(|value| value.unwrap_or_else(now_ms))
}

pub(super) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX)
}

pub(super) fn flag(input: &Value, field: &str, default: bool) -> Result<bool> {
    match input.get(field) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Bool(value)) => Ok(*value),
        _ => Err(errors::invalid(format!("{field} must be a boolean"))),
    }
}

pub(super) fn nonnegative_i64(input: &Value, field: &str) -> Result<i64> {
    let value = required_i64(input, field)?;
    if value < 0 {
        return Err(errors::invalid(format!("{field} must be nonnegative")));
    }
    Ok(value)
}

pub(super) fn string_array(input: &Value, field: &str) -> Result<Vec<String>> {
    input[field]
        .as_array()
        .ok_or_else(|| errors::invalid(format!("{field} must be an array")))
        .and_then(|items| {
            items
                .iter()
                .map(|item| {
                    item.as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| errors::invalid(format!("{field} must hold strings")))
                })
                .collect()
        })
}

/// The store record's serde JSON, or `null` for absence — the shape every
/// existing port arm already renders, so the TypeScript adapter consumes
/// records rather than Rust debug output.
pub(super) fn record<T: serde::Serialize>(value: T) -> Value {
    serde_json::to_value(value).ok().unwrap_or(Value::Null)
}

pub(super) fn records<T: serde::Serialize>(values: Vec<T>) -> Value {
    Value::Array(values.into_iter().map(record).collect())
}

pub(super) fn optional_record<T: serde::Serialize>(value: Option<T>) -> Value {
    value.map(record).unwrap_or(Value::Null)
}

/// Resolve the port's `{taskId, leaseToken}` write fence into a full fencing
/// proof. The fencing token is the store's fact: it is read from the current
/// lease and the fenced engine write re-verifies every field atomically, so
/// a stale or forged token loses exactly as the store's own checks demand.
pub(super) async fn fence_proof(
    store: &SqliteStore,
    task_id: &str,
    lease_token: &str,
) -> Result<LeaseFence> {
    let lease = store
        .get_lease(task_id)
        .await
        .map_err(errors::store)?
        .ok_or_else(|| errors::store(agentkit_store::StoreError::LeaseLost(task_id.to_owned())))?;
    Ok(LeaseFence {
        task_id: task_id.to_owned(),
        lease_token: lease_token.to_owned(),
        fencing_token: lease.fencing_token,
    })
}

pub(super) async fn optional_fence(
    store: &SqliteStore,
    input: &Value,
    field: &str,
) -> Result<Option<LeaseFence>> {
    match input.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(_)) => {
            let fence = &input[field];
            let task_id = text(fence, "taskId")?;
            let lease_token = text(fence, "leaseToken")?;
            fence_proof(store, task_id, lease_token).await.map(Some)
        }
        _ => Err(errors::invalid(format!("{field} must be an object"))),
    }
}

/// The `opts.leaseToken` shape `FencedWriteOptions` carries.
pub(super) async fn opt_lease_token(input: &Value) -> Result<Option<String>> {
    match input.get("opts") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(_)) => match &input["opts"].get("leaseToken") {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(token)) => Ok(Some(token.clone())),
            _ => Err(errors::invalid("opts.leaseToken must be a string")),
        },
        _ => Err(errors::invalid("opts must be an object")),
    }
}

/// The required `opts.leaseToken` shape (`AppendEventsOptions`,
/// `UpdateProgressOptions`): no opts or a missing token is malformed input.
pub(super) async fn required_lease_token(input: &Value) -> Result<String> {
    opt_lease_token(input)
        .await?
        .ok_or_else(|| errors::invalid("opts.leaseToken must be a string"))
}

pub(super) fn deserialized<T: serde::de::DeserializeOwned>(
    input: &Value,
    field: &str,
) -> Result<T> {
    serde_json::from_value(input[field].clone())
        .map_err(|error| errors::invalid(format!("invalid {field}: {error}")))
}

async fn providers(store: &SqliteStore, operation: &str, input: Value) -> Result<Value> {
    let result = match operation {
        "list" => Value::Array(store.list_providers().await.map_err(errors::store)?),
        "get" => store
            .get_provider(text(&input, "providerId")?)
            .await
            .map_err(errors::store)?
            .unwrap_or(Value::Null),
        "upsert" => store.upsert_provider(input).await.map_err(errors::store)?,
        "delete" => {
            store
                .delete_provider(text(&input, "providerId")?)
                .await
                .map_err(errors::store)?;
            Value::Null
        }
        _ => return catalogue(store, operation, input).await,
    };
    Ok(result)
}

async fn catalogue(store: &SqliteStore, operation: &str, input: Value) -> Result<Value> {
    let result = match operation {
        "listModels" => Value::Array(
            store
                .list_provider_models(text(&input, "providerId")?)
                .await
                .map_err(errors::store)?,
        ),
        "replaceModels" => {
            let models = input["models"]
                .as_array()
                .ok_or_else(|| errors::invalid("models must be an array"))?
                .clone();
            store
                .replace_provider_models(text(&input, "providerId")?, models)
                .await
                .map_err(errors::store)?;
            Value::Null
        }
        "getCapabilities" => store
            .get_provider_capabilities(text(&input, "providerId")?)
            .await
            .map_err(errors::store)?
            .unwrap_or(Value::Null),
        "saveCapabilities" => {
            store
                .save_provider_capabilities(
                    text(&input, "providerId")?,
                    input["capabilities"].clone(),
                )
                .await
                .map_err(errors::store)?;
            Value::Null
        }
        _ => {
            return Err(errors::stable(
                "not_implemented",
                "Unsupported provider store operation",
                None,
            ));
        }
    };
    Ok(result)
}

async fn summaries(store: &SqliteStore, operation: &str, input: Value) -> Result<Value> {
    match operation {
        "save" => {
            store
                .save_context_summary(input)
                .await
                .map_err(errors::store)?;
            Ok(Value::Null)
        }
        "list" => Ok(Value::Array(
            store
                .list_context_summaries(text(&input, "sessionId")?)
                .await
                .map_err(errors::store)?,
        )),
        "deleteSession" => Ok(json!(
            store
                .delete_session_context_summaries(text(&input, "sessionId")?)
                .await
                .map_err(errors::store)?
        )),
        _ => Err(errors::stable(
            "not_implemented",
            "Unsupported context summary store operation",
            None,
        )),
    }
}

async fn chats(store: &SqliteStore, operation: &str, input: Value) -> Result<Value> {
    match operation {
        "get" => Ok(store
            .get_chat(text(&input, "chatId")?)
            .await
            .map_err(errors::store)?
            .and_then(|chat| serde_json::to_value(chat).ok())
            .unwrap_or(Value::Null)),
        "list" => {
            let include_archived = match input.get("includeArchived") {
                None | Some(Value::Null) => false,
                Some(Value::Bool(value)) => *value,
                _ => {
                    return Err(errors::invalid("includeArchived must be a boolean"));
                }
            };
            Ok(Value::Array(
                store
                    .list_chats(include_archived)
                    .await
                    .map_err(errors::store)?
                    .into_iter()
                    .filter_map(|chat| serde_json::to_value(chat).ok())
                    .collect(),
            ))
        }
        _ => Err(errors::stable(
            "not_implemented",
            "Unsupported chat store operation",
            None,
        )),
    }
}

#[napi]
impl NativeStore {
    /// Internal JSON port used by the compatibility facade. Rust owns validation,
    /// persistence, ordering, and atomicity; unknown operations never run effects.
    #[napi]
    pub async fn call_store(&self, operation: String, input_json: String) -> Result<String> {
        execute(self.handle()?, &operation, &input_json).await
    }
}

pub(super) async fn execute(
    store: SqliteStore,
    operation: &str,
    input_json: &str,
) -> Result<String> {
    let input = agentkit_contracts::parse_javascript_json(input_json)
        .map_err(|error| errors::invalid(error.to_string()))?;
    let Some((namespace, method)) = operation.split_once('.') else {
        return Err(errors::invalid(
            "Store operation must include its namespace",
        ));
    };
    let value = match namespace {
        "providers" => providers(&store, method, input).await?,
        "summaries" => summaries(&store, method, input).await?,
        "chats" => chats(&store, method, input).await?,
        "conversations" => super::conversations::execute(&store, method, input).await?,
        "tasks" => super::tasks::execute(&store, method, input).await?,
        "proposals" => super::proposals::execute(&store, method, input).await?,
        "inputs" => super::inputs::execute(&store, method, input).await?,
        "settings" => super::settings::execute(&store, method, input).await?,
        "outbox" => super::outbox::execute(&store, method, input).await?,
        "continuations" => super::continuations::execute(&store, method, input).await?,
        "toolExecutions" => super::tool_executions::execute(&store, method, input).await?,
        _ => {
            return Err(errors::stable(
                "not_implemented",
                "Unsupported native store port",
                None,
            ));
        }
    };
    serde_json::to_string(&value).map_err(|error| errors::internal(&error.to_string()))
}

/// Shared harness for the namespace test modules: every port test speaks the
/// exact wire contract (`operation` + JSON string in, JSON string out) so it
/// exercises dispatch, parsing, serialization, and the store as one surface.
#[cfg(test)]
pub mod testkit {
    use agentkit_store::{SqliteStore, StoreTransaction};
    use serde_json::Value;

    pub const NOW: i64 = 1_760_000_000_000;

    pub fn store() -> SqliteStore {
        SqliteStore::memory(16).expect("in-memory store")
    }

    pub async fn call(
        store: &SqliteStore,
        operation: &str,
        input: Value,
    ) -> Result<Value, napi::Error> {
        let payload = super::execute(store.clone(), operation, &input.to_string()).await?;
        Ok(serde_json::from_str(&payload).expect("port output is JSON"))
    }

    pub fn code(error: napi::Error) -> String {
        serde_json::from_str::<Value>(&error.reason).expect("error payload is JSON")["code"]
            .as_str()
            .expect("payload carries a code")
            .to_owned()
    }

    pub async fn begin(store: &SqliteStore) -> StoreTransaction {
        store.begin_transaction().await.expect("transaction opened")
    }
}
