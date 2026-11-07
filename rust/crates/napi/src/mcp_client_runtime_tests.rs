use super::*;

fn record(alias: &str) -> McpServerDto {
    serde_json::from_value(json!({"id":"server", "alias":alias,
        "transport":{"kind":"http","url":"https://resource.test/mcp"},
        "createdAt":"2026-11-05T00:00:00Z", "updatedAt":"2026-11-05T00:00:00Z"}))
    .unwrap()
}

#[tokio::test]
async fn preflight_reservation_fences_admission_and_drop_releases_it() {
    let store = NativeStore::memory().unwrap();
    let (runtime, _) = store.mcp_config_ports().await.unwrap();
    let lease = runtime.prepare(1, vec![record("reserved")]).await.unwrap();
    assert!(store.admit_connect("reserved", None, None).await.is_err());
    assert!(store.mcp_shared.lock().unwrap().pending.is_empty());
    drop(lease);
    let candidate = store.admit_connect("reserved", None, None).await.unwrap();
    assert!(matches!(
        runtime.prepare(2, vec![record("reserved")]).await,
        Err(McpConfigError::Conflict)
    ));
    assert!(store.mcp_shared.lock().unwrap().reservation.is_none());
    store.abandon_connect("reserved", &candidate.id).await;
    let lease = runtime.prepare(3, vec![record("reserved")]).await.unwrap();
    drop(lease);
    store.close().await.unwrap();
}

#[tokio::test]
async fn rejected_preflight_keeps_manual_declaration_and_generation() {
    let store = NativeStore::memory().unwrap();
    let (runtime, _) = store.mcp_config_ports().await.unwrap();
    let mut original = declarations(vec![record("manual")])
        .unwrap()
        .remove("manual")
        .unwrap();
    original.transport = TransportConfig::Http {
        url: "https://original.test/mcp".into(),
        headers: BTreeMap::new(),
    };
    {
        let mut guard = store.mcp.lock().await;
        let registry = guard.as_mut().unwrap();
        registry
            .manager
            .replace_configs(vec![original.clone()])
            .await
            .unwrap();
        registry.configs.insert("manual".into(), original.clone());
    }
    assert!(matches!(
        runtime.prepare(1, vec![record("manual")]).await,
        Err(McpConfigError::Conflict)
    ));
    let guard = store.mcp.lock().await;
    assert!(guard.as_ref().unwrap().configs.get("manual") == Some(&original));
    {
        let shared = store.mcp_shared.lock().unwrap();
        assert_eq!(shared.generation, 0);
        assert!(shared.reservation.is_none());
        assert!(shared.blocked.is_empty());
    }
    drop(guard);
    store.close().await.unwrap();
}

#[tokio::test]
async fn changed_owned_pending_candidate_is_cancelled_before_reconcile() {
    let store = NativeStore::memory().unwrap();
    let (runtime, _) = store.mcp_config_ports().await.unwrap();
    let initial = record("owned");
    let lease = runtime.prepare(1, vec![initial.clone()]).await.unwrap();
    runtime.invalidate(1, &["owned".into()]);
    runtime.reconcile(1, vec![initial.clone()]).await.unwrap();
    drop(lease);
    let candidate = store.admit_connect("owned", None, None).await.unwrap();
    let mut next = serde_json::to_value(initial).unwrap();
    next["enabled"] = json!(false);
    let next: McpServerDto = serde_json::from_value(next).unwrap();
    let lease = runtime.prepare(2, vec![next.clone()]).await.unwrap();
    runtime.invalidate(2, &["owned".into()]);
    assert!(candidate.token.is_cancelled());
    assert!(store.admit_connect("unrelated", None, None).await.is_err());
    store.abandon_connect("owned", &candidate.id).await;
    runtime.reconcile(2, vec![next]).await.unwrap();
    drop(lease);
    assert!(store.mcp_shared.lock().unwrap().reservation.is_none());
    assert!(!store.mcp.lock().await.as_ref().unwrap().configs["owned"].enabled);
    store.close().await.unwrap();
}
