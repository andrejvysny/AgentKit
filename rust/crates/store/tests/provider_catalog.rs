use agentkit_store::*;
use serde_json::{Value, json};

fn config(id: &str) -> Value {
    json!({"id":id,"label":id,"kind":"custom","baseUrl":"https://example.invalid","defaultModel":"m","enabled":true,"apiKey":"secret-ref","metadata":{"nested":[1,2]},"extraHeaders":{"X-Test":"value"}})
}
fn model(provider: &str, id: &str) -> Value {
    json!({"providerId":provider,"modelId":id,"displayName":null,"fetchedAt":"2026-01-01T00:00:00Z"})
}
fn capabilities() -> Value {
    json!({"streaming":true,"toolCalling":false,"modelList":true,"audio":true,"maxContextTokens":1000})
}

async fn exercise(store: &SqliteStore) {
    let first = config("z");
    assert_eq!(store.upsert_provider(first.clone()).await.unwrap(), first);
    store.upsert_provider(config("a")).await.unwrap();
    let mut updated = config("z");
    updated["label"] = json!("updated");
    store.upsert_provider(updated.clone()).await.unwrap();
    assert_eq!(
        store.list_providers().await.unwrap(),
        vec![updated.clone(), config("a")]
    );
    assert_eq!(store.get_provider("z").await.unwrap(), Some(updated));
    assert_eq!(store.get_provider("missing").await.unwrap(), None);
    store
        .replace_provider_models("z", vec![model("z", "z"), model("z", "a")])
        .await
        .unwrap();
    assert_eq!(
        store.list_provider_models("z").await.unwrap(),
        vec![model("z", "a"), model("z", "z")]
    );
    assert!(
        store
            .replace_provider_models("z", vec![model("z", "a"), model("z", "a")])
            .await
            .is_err()
    );
    assert_eq!(store.list_provider_models("z").await.unwrap().len(), 2);
    store
        .save_provider_capabilities("z", capabilities())
        .await
        .unwrap();
    assert_eq!(
        store.get_provider_capabilities("z").await.unwrap(),
        Some(capabilities())
    );
}

#[tokio::test]
async fn providers_work_in_memory_and_delete_is_scoped() {
    let store = SqliteStore::memory(8).unwrap();
    exercise(&store).await;
    store.delete_provider("z").await.unwrap();
    assert_eq!(store.get_provider("z").await.unwrap(), None);
    assert!(store.list_provider_models("z").await.unwrap().is_empty());
    assert_eq!(store.get_provider_capabilities("z").await.unwrap(), None);
    assert_eq!(store.list_providers().await.unwrap(), vec![config("a")]);
}

#[tokio::test]
async fn providers_survive_reopen_and_malformed_records_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.db");
    let store = SqliteStore::open(&path, 8).unwrap();
    exercise(&store).await;
    drop(store);
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "provider_reopen_child"])
        .env("AGENTKIT_PROVIDER_DB", &path)
        .output()
        .unwrap();
    assert!(
        child.status.success(),
        "{}",
        String::from_utf8_lossy(&child.stderr)
    );
    let reopened = SqliteStore::open(&path, 8).unwrap();
    assert_eq!(
        reopened.get_provider("z").await.unwrap().unwrap()["label"],
        "updated"
    );
    assert_eq!(reopened.list_provider_models("z").await.unwrap().len(), 2);
    assert_eq!(
        reopened.get_provider_capabilities("z").await.unwrap(),
        Some(capabilities())
    );
    reopened
        .set_setting(PROVIDER_MODELS_NAMESPACE, "z", json!([{"bad":true}]))
        .await
        .unwrap();
    assert!(reopened.list_provider_models("z").await.is_err());
    reopened
        .set_setting(PROVIDERS_NAMESPACE, "bad", json!({"id":"bad"}))
        .await
        .unwrap();
    assert!(reopened.list_providers().await.is_err());
}

#[test]
#[ignore]
fn provider_reopen_child() {
    let path = std::env::var_os("AGENTKIT_PROVIDER_DB").unwrap();
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(async {
            let store = SqliteStore::open(path, 8).unwrap();
            assert_eq!(
                store.get_provider("z").await.unwrap().unwrap()["label"],
                "updated"
            );
            assert_eq!(store.list_provider_models("z").await.unwrap().len(), 2);
            assert_eq!(
                store.get_provider_capabilities("z").await.unwrap(),
                Some(capabilities())
            );
        });
}
