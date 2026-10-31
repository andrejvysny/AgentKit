use super::NativeStore;
use crate::errors;
use agentkit_store::SqliteStore;
use napi::bindgen_prelude::*;
use serde_json::{Value, json};

fn text<'a>(input: &'a Value, field: &str) -> Result<&'a str> {
    input[field]
        .as_str()
        .ok_or_else(|| errors::invalid(format!("{field} must be a string")))
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
