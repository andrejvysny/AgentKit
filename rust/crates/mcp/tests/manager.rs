#[path = "support/interop.rs"]
mod interop;
#[path = "support/managed.rs"]
mod support;
use agentkit_mcp::*;
use serde_json::json;
use std::sync::{Arc, atomic::Ordering};
use support::*;

#[tokio::test]
async fn connect_cycle_backoff_and_hard_circuit_match_typescript() {
    let live = Arc::new(Connection::default());
    let (manager, _, connector, clock) = manager(
        vec![config("docs")],
        vec![
            Plan::Fail(McpClientError::Connect),
            Plan::Fail(McpClientError::Connect),
            Plan::Fail(McpClientError::Connect),
            Plan::Client(live),
        ],
    );
    assert_eq!(
        manager.connect("docs", token()).await,
        Err(McpClientError::Connect)
    );
    assert_eq!(*clock.sleeps.lock().unwrap(), [250, 500]);
    assert_eq!(
        manager.connect("docs", token()).await,
        Err(McpClientError::CircuitOpen)
    );
    clock.now.fetch_add(4999, Ordering::SeqCst);
    assert_eq!(
        manager.connect("docs", token()).await,
        Err(McpClientError::CircuitOpen)
    );
    assert_eq!(connector.attempts.load(Ordering::SeqCst), 3);
    clock.now.fetch_add(1, Ordering::SeqCst);
    manager.connect("docs", token()).await.unwrap();
    assert_eq!(connector.attempts.load(Ordering::SeqCst), 4);
    manager.dispose().await.unwrap();
}

#[tokio::test]
async fn missing_secret_is_not_retried_or_hidden_by_a_circuit() {
    let mut declaration = config("docs");
    declaration
        .secret_refs
        .insert("token".into(), "secret-ref".into());
    let (manager, secrets, connector, _) = manager(
        vec![declaration],
        vec![Plan::Client(Arc::new(Connection::default()))],
    );
    assert_eq!(
        manager.connect("docs", token()).await,
        Err(McpClientError::SecretMissing)
    );
    assert_eq!(connector.attempts.load(Ordering::SeqCst), 0);
    secrets
        .values
        .lock()
        .unwrap()
        .insert("secret-ref".into(), "private".into());
    manager.connect("docs", token()).await.unwrap();
    assert_eq!(connector.attempts.load(Ordering::SeqCst), 1);
    manager.dispose().await.unwrap();
}

#[tokio::test]
async fn partial_contribution_keeps_healthy_tools_and_routes_effective_aliases() {
    let mut broken = config("broken");
    broken.resilience.max_connect_attempts = 1;
    let mut disabled = config("disabled");
    disabled.enabled = false;
    let mut good = config("good");
    good.tool_aliases
        .insert("remote".into(), "search.query".into());
    let live = Arc::new(Connection::default());
    let (manager, _, _, _) = manager(
        vec![broken, disabled, good],
        vec![
            Plan::Fail(McpClientError::Connect),
            Plan::Client(live.clone()),
        ],
    );
    let report = manager.contribute(token()).await.unwrap();
    assert_eq!(report.connections.connected, ["good"]);
    assert_eq!(report.connections.skipped, ["disabled"]);
    assert_eq!(
        report.connections.failed,
        [("broken".into(), McpClientError::Connect)]
    );
    assert_eq!(
        report.tools[0].definition().name,
        "mcp__good__search__query"
    );
    manager
        .call_tool("mcp.good.search.query", model::JsonObject::new(), token())
        .await
        .unwrap();
    assert_eq!(*live.names.lock().unwrap(), ["remote"]);
    manager.dispose().await.unwrap();
}

#[tokio::test]
async fn concurrent_failed_reads_share_one_reconnect_generation() {
    let broken = Arc::new(Connection {
        barrier: Some(Arc::new(tokio::sync::Barrier::new(2))),
        ..Connection::default()
    });
    broken.lists.lock().unwrap().extend([
        Err(McpClientError::Disconnected),
        Err(McpClientError::Disconnected),
    ]);
    let live = Arc::new(Connection::default());
    let (manager, _, connector, _) = manager(
        vec![config("docs")],
        vec![Plan::Client(broken.clone()), Plan::Client(live)],
    );
    let (a, b) = tokio::join!(
        manager.list_tools("docs", token()),
        manager.list_tools("docs", token())
    );
    assert_eq!(a.unwrap()[0].name, "remote");
    assert_eq!(b.unwrap()[0].name, "remote");
    assert_eq!(connector.attempts.load(Ordering::SeqCst), 2);
    assert_eq!(broken.closed.load(Ordering::SeqCst), 1);
    manager.dispose().await.unwrap();
}

#[tokio::test]
async fn ambiguous_calls_never_replay_even_when_timeout_retries_are_enabled() {
    for error in [
        McpClientError::Disconnected,
        McpClientError::Timeout,
        McpClientError::Cancelled,
        McpClientError::Request,
    ] {
        let connection = Arc::new(Connection::default());
        connection
            .calls
            .lock()
            .unwrap()
            .push_back(Err(error.clone()));
        let mut declaration = config("docs");
        declaration.resilience.retry_timeouts = true;
        let (manager, _, connector, _) =
            manager(vec![declaration], vec![Plan::Client(connection.clone())]);
        assert_eq!(
            manager
                .call_tool("mcp.docs.remote", model::JsonObject::new(), token())
                .await,
            Err(error)
        );
        assert_eq!(connection.effects.load(Ordering::SeqCst), 1);
        assert_eq!(connector.attempts.load(Ordering::SeqCst), 1);
        manager.dispose().await.unwrap();
    }
}

#[tokio::test]
async fn rotation_resolves_new_material_and_invalidates_failed_circuit() {
    let mut declaration = config("docs");
    declaration.secret_refs.insert("token".into(), "ref".into());
    declaration.resilience.max_connect_attempts = 1;
    let live = Arc::new(Connection::default());
    let (manager, secrets, connector, clock) = manager(
        vec![declaration],
        vec![Plan::Fail(McpClientError::Connect), Plan::Client(live)],
    );
    secrets
        .values
        .lock()
        .unwrap()
        .insert("ref".into(), "old-secret".into());
    assert_eq!(
        manager.connect("docs", token()).await,
        Err(McpClientError::Connect)
    );
    secrets
        .values
        .lock()
        .unwrap()
        .insert("ref".into(), "new-secret".into());
    secrets.revision.fetch_add(1, Ordering::SeqCst);
    manager.connect("docs", token()).await.unwrap();
    assert_eq!(clock.now.load(Ordering::SeqCst), 0);
    {
        let transports = connector.transports.lock().unwrap();
        assert!(
            matches!(&transports[0], TransportConfig::Stdio {env, ..} if env["TOKEN"] == "old-secret")
        );
        assert!(
            matches!(&transports[1], TransportConfig::Stdio {env, ..} if env["TOKEN"] == "new-secret")
        );
    }
    manager.dispose().await.unwrap();
}

#[tokio::test]
async fn config_replacement_and_dispose_revoke_retained_tool_handles() {
    let first = Arc::new(Connection::default());
    let second = Arc::new(Connection::default());
    let declaration = config("docs");
    let (manager, _, _, _) = manager(
        vec![declaration.clone()],
        vec![Plan::Client(first.clone()), Plan::Client(second.clone())],
    );
    let old = manager.contribute(token()).await.unwrap().tools.remove(0);
    let mut changed = declaration;
    changed
        .secret_refs
        .insert("unused".into(), "new-ref".into());
    changed.transport = TransportConfig::Http {
        url: "https://example.invalid/mcp".into(),
        headers: Default::default(),
    };
    changed.secret_refs.clear();
    manager.replace_configs(vec![changed]).await.unwrap();
    let result = old.execute(context(), json!({})).await.unwrap();
    assert!(!result.ok);
    assert_eq!(result.model_data.unwrap()["errorCode"], "mcp_not_connected");
    assert_eq!(first.effects.load(Ordering::SeqCst), 0);
    assert_eq!(first.closed.load(Ordering::SeqCst), 1);
    let current = manager.contribute(token()).await.unwrap().tools.remove(0);
    manager.dispose().await.unwrap();
    assert!(!current.execute(context(), json!({})).await.unwrap().ok);
    assert_eq!(second.effects.load(Ordering::SeqCst), 0);
    assert_eq!(
        manager.connect("docs", token()).await,
        Err(McpClientError::Closed)
    );
}

#[tokio::test]
async fn close_wins_pending_connect_and_explicit_connect_starts_new_lifecycle() {
    let connection = Arc::new(Connection::default());
    let (manager, _, connector, _) = manager(
        vec![config("docs")],
        vec![Plan::Hold, Plan::Client(connection.clone())],
    );
    let pending = tokio::spawn({
        let manager = manager.clone();
        async move { manager.connect("docs", token()).await }
    });
    connector.started.notified().await;
    manager.close("docs").await.unwrap();
    assert_eq!(pending.await.unwrap(), Err(McpClientError::Closed));
    assert_eq!(connector.dropped.load(Ordering::SeqCst), 1);
    assert!(manager.connected_aliases().is_empty());
    manager.connect("docs", token()).await.unwrap();
    manager.dispose().await.unwrap();
    assert_eq!(connection.closed.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cancelled_or_timed_out_inflight_call_has_one_effect_and_no_reconnect() {
    let connection = Arc::new(Connection {
        hold_call: true,
        ..Connection::default()
    });
    let mut declaration = config("docs");
    declaration.resilience.request_timeout_ms = 10;
    let (manager, _, connector, _) =
        manager(vec![declaration], vec![Plan::Client(connection.clone())]);
    assert_eq!(
        manager
            .call_tool("mcp.docs.remote", model::JsonObject::new(), token())
            .await,
        Err(McpClientError::Timeout)
    );
    let cancel = token();
    let task = tokio::spawn({
        let manager = manager.clone();
        let cancel = cancel.clone();
        async move {
            manager
                .call_tool("mcp.docs.remote", model::JsonObject::new(), cancel)
                .await
        }
    });
    // The first timeout left one stored notification; clear it before awaiting the second call.
    connection.call_started.notified().await;
    while connection.effects.load(Ordering::SeqCst) < 2 {
        tokio::task::yield_now().await;
    }
    cancel.cancel();
    assert_eq!(task.await.unwrap(), Err(McpClientError::Cancelled));
    assert_eq!(connection.effects.load(Ordering::SeqCst), 2);
    assert_eq!(connector.attempts.load(Ordering::SeqCst), 1);
    manager.dispose().await.unwrap();
}

#[tokio::test]
async fn default_manager_runs_real_stdio_with_curated_inherited_path() {
    if interop::ts_interop_skipped("mcp manager stdio fixture") {
        return;
    }
    let mut declaration = config("docs");
    declaration.transport = TransportConfig::Stdio {
        command: "bun".into(),
        args: vec![format!(
            "{}/tests/fixtures/stdio.mjs",
            env!("CARGO_MANIFEST_DIR")
        )],
        env: Default::default(),
    };
    let manager = McpClientManager::new(vec![declaration], Arc::new(Secrets::default())).unwrap();
    assert_eq!(manager.list_tools("docs", token()).await.unwrap().len(), 2);
    manager.dispose().await.unwrap();
}

#[tokio::test]
async fn live_secret_rotation_closes_old_client_before_dispatching_again() {
    let mut declaration = config("docs");
    declaration.secret_refs.insert("token".into(), "ref".into());
    let (first, second) = (
        Arc::new(Connection::default()),
        Arc::new(Connection::default()),
    );
    let (manager, secrets, connector, _) = manager(
        vec![declaration.clone()],
        vec![Plan::Client(first.clone()), Plan::Client(second.clone())],
    );
    secrets
        .values
        .lock()
        .unwrap()
        .insert("ref".into(), "old-private".into());
    manager.list_tools("docs", token()).await.unwrap();
    secrets
        .values
        .lock()
        .unwrap()
        .insert("ref".into(), "new-private".into());
    secrets.revision.fetch_add(1, Ordering::SeqCst);
    manager
        .call_tool("mcp.docs.remote", model::JsonObject::new(), token())
        .await
        .unwrap();
    assert_eq!(first.closed.load(Ordering::SeqCst), 1);
    assert_eq!(first.effects.load(Ordering::SeqCst), 0);
    assert_eq!(second.effects.load(Ordering::SeqCst), 1);
    assert_eq!(connector.attempts.load(Ordering::SeqCst), 2);
    let persisted = serde_json::to_string(&declaration).unwrap();
    assert!(!persisted.contains("old-private") && !persisted.contains("new-private"));
    manager.dispose().await.unwrap();
}

#[tokio::test]
async fn read_reconnect_exhaustion_has_exact_attempt_budget_and_backoff() {
    let clients: Vec<_> = (0..3).map(|_| Arc::new(Connection::default())).collect();
    for client in &clients {
        client
            .lists
            .lock()
            .unwrap()
            .push_back(Err(McpClientError::Disconnected));
    }
    let (manager, _, connector, clock) = manager(
        vec![config("docs")],
        clients.iter().cloned().map(Plan::Client).collect(),
    );
    assert!(matches!(
        manager.list_tools("docs", token()).await,
        Err(McpClientError::ReconnectExhausted)
    ));
    assert_eq!(connector.attempts.load(Ordering::SeqCst), 3);
    assert_eq!(*clock.sleeps.lock().unwrap(), [250, 500]);
    assert!(
        clients
            .iter()
            .all(|client| client.closed.load(Ordering::SeqCst) == 1)
    );
    manager.dispose().await.unwrap();
}

#[tokio::test]
async fn timeout_opt_in_only_replays_catalogue_reads() {
    for retry in [false, true] {
        let first = Arc::new(Connection::default());
        first
            .lists
            .lock()
            .unwrap()
            .push_back(Err(McpClientError::Timeout));
        let mut declaration = config("docs");
        declaration.resilience.retry_timeouts = retry;
        let (manager, _, connector, _) = manager(
            vec![declaration],
            vec![
                Plan::Client(first),
                Plan::Client(Arc::new(Connection::default())),
            ],
        );
        let result = manager.list_tools("docs", token()).await;
        assert_eq!(result.is_ok(), retry);
        assert_eq!(
            connector.attempts.load(Ordering::SeqCst),
            if retry { 2 } else { 1 }
        );
        manager.dispose().await.unwrap();
    }
}

#[tokio::test]
async fn provably_unsent_call_can_reconnect_without_duplicating_effects() {
    let first = Arc::new(Connection {
        reject_before_dispatch: true,
        ..Connection::default()
    });
    let second = Arc::new(Connection::default());
    let (manager, _, connector, _) = manager(
        vec![config("docs")],
        vec![Plan::Client(first.clone()), Plan::Client(second.clone())],
    );
    manager
        .call_tool("mcp.docs.remote", model::JsonObject::new(), token())
        .await
        .unwrap();
    assert_eq!(connector.attempts.load(Ordering::SeqCst), 2);
    assert_eq!(first.effects.load(Ordering::SeqCst), 0);
    assert_eq!(second.effects.load(Ordering::SeqCst), 1);
    manager.dispose().await.unwrap();
}

#[tokio::test]
async fn hung_connects_hit_deadlines_release_futures_and_open_circuit() {
    let mut declaration = config("docs");
    declaration.resilience.connect_timeout_ms = 2;
    let (manager, _, connector, clock) =
        manager(vec![declaration], vec![Plan::Hold, Plan::Hold, Plan::Hold]);
    assert_eq!(
        manager.connect("docs", token()).await,
        Err(McpClientError::Connect)
    );
    assert_eq!(connector.attempts.load(Ordering::SeqCst), 3);
    assert_eq!(connector.dropped.load(Ordering::SeqCst), 3);
    assert_eq!(*clock.sleeps.lock().unwrap(), [250, 500]);
    assert_eq!(
        manager.connect("docs", token()).await,
        Err(McpClientError::CircuitOpen)
    );
    assert!(manager.connected_aliases().is_empty());
    manager.dispose().await.unwrap();
}
