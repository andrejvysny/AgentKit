//! The `settings.*` port surface: the single assistant-settings document.
//! Same storage the Rust host's HTTP lane already uses — namespace `http`,
//! key `settings` — so both lanes read one document. Defaults and the patch
//! whitelist mirror `agentkit-host`'s settings handler exactly; the store
//! keeps the JSON verbatim.

use super::ports::record;
use crate::errors;
use agentkit_store::SqliteStore;
use napi::bindgen_prelude::*;
use serde_json::{Value, json};

const NAMESPACE: &str = "http";
const KEY: &str = "settings";
const PATCHABLE: [&str; 8] = [
    "defaultProviderId",
    "defaultModel",
    "contextSizePreference",
    "writePolicyMode",
    "allowRawToolData",
    "maxToolIterations",
    "toolCalling",
    "metadata",
];

pub(super) async fn execute(store: &SqliteStore, method: &str, input: Value) -> Result<Value> {
    match method {
        "getSettings" => Ok(current(store).await?),
        "updateSettings" => {
            let patch = match input.get("patch") {
                Some(Value::Object(_)) => input["patch"].clone(),
                None | Some(Value::Null) => json!({}),
                _ => return Err(errors::invalid("patch must be an object")),
            };
            let mut value = current(store).await?;
            if let Some(patch) = patch.as_object() {
                for key in PATCHABLE {
                    if let Some(entry) = patch.get(key) {
                        value[key] = entry.clone();
                    }
                }
            }
            store
                .set_setting(NAMESPACE, KEY, value.clone())
                .await
                .map_err(errors::store)?;
            Ok(value)
        }
        _ => Err(errors::stable(
            "not_implemented",
            "Unsupported settings store operation",
            None,
        )),
    }
}

async fn current(store: &SqliteStore) -> Result<Value> {
    Ok(store
        .get_setting(NAMESPACE, KEY)
        .await
        .map_err(errors::store)?
        .unwrap_or_else(defaults))
}

fn defaults() -> Value {
    record(json!({
        "contextSizePreference": "small",
        "writePolicyMode": "auto_readonly_confirm_writes",
        "allowRawToolData": false,
        "metadata": {},
    }))
}

#[cfg(test)]
mod tests {
    use crate::store::ports::testkit::{begin, call, code, store};
    use serde_json::json;

    #[tokio::test]
    async fn settings_default_merge_and_whitelist() {
        let store = store();
        let defaults = call(&store, "settings.getSettings", json!({}))
            .await
            .expect("defaults");
        assert_eq!(defaults["contextSizePreference"], "small");
        assert_eq!(defaults["writePolicyMode"], "auto_readonly_confirm_writes");
        assert_eq!(defaults["allowRawToolData"], false);

        let merged = call(
            &store,
            "settings.updateSettings",
            json!({"patch":{"defaultProviderId":"prov-1","defaultModel":"model-x","notWhitelisted":"nope"}}),
        )
        .await
        .expect("patched");
        assert_eq!(merged["defaultProviderId"], "prov-1");
        assert_eq!(
            merged["contextSizePreference"], "small",
            "unpatched fields persist"
        );
        assert!(
            merged.get("notWhitelisted").is_none(),
            "the whitelist is the gate"
        );

        assert_eq!(
            call(&store, "settings.getSettings", json!({}))
                .await
                .unwrap()["defaultProviderId"],
            "prov-1",
            "the write persisted"
        );
    }

    #[tokio::test]
    async fn malformed_patch_is_a_typed_error() {
        let store = store();
        assert_eq!(
            code(
                call(&store, "settings.updateSettings", json!({"patch":"yes"}))
                    .await
                    .unwrap_err()
            ),
            "invalid_input"
        );
        assert_eq!(
            code(
                call(&store, "settings.resetDefaults", json!({}))
                    .await
                    .unwrap_err()
            ),
            "not_implemented"
        );
    }

    #[tokio::test]
    async fn committed_settings_update_becomes_visible() {
        let store = store();
        let transaction = begin(&store).await;
        call(
            &transaction.view(),
            "settings.updateSettings",
            json!({"patch":{"maxToolIterations":9}}),
        )
        .await
        .expect("patch inside the transaction");
        transaction.commit().await.expect("commit");
        assert_eq!(
            call(&store, "settings.getSettings", json!({}))
                .await
                .unwrap()["maxToolIterations"],
            9
        );
    }

    #[tokio::test]
    async fn rolled_back_settings_update_leaves_the_document_untouched() {
        let store = store();
        let transaction = begin(&store).await;
        call(
            &transaction.view(),
            "settings.updateSettings",
            json!({"patch":{"defaultProviderId":"doomed"}}),
        )
        .await
        .expect("patch inside the transaction");
        transaction.rollback().await.expect("rollback");
        let settings = call(&store, "settings.getSettings", json!({}))
            .await
            .expect("defaults");
        assert!(settings.get("defaultProviderId").is_none());
    }
}
