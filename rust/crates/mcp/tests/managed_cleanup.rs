#[path = "support/managed.rs"]
mod support;
use agentkit_mcp::*;
use std::{sync::Arc, time::Duration};
use support::token;
// Only the unix-gated hung-stdio test constructs a Secrets store; an
// unconditional import is dead code on Windows and dies under -D warnings.
#[cfg(unix)]
use support::Secrets;

#[tokio::test]
async fn manager_drop_revokes_retained_tools_and_closes_their_transport() {
    let connection = Arc::new(support::Connection::default());
    let (manager, _, _, _) = support::manager(
        vec![support::config("docs")],
        vec![support::Plan::Client(connection.clone())],
    );
    let retained = manager.contribute(token()).await.unwrap().tools.remove(0);
    drop(manager);
    tokio::time::timeout(Duration::from_secs(1), async {
        while connection.closed.load(std::sync::atomic::Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        !retained
            .execute(support::context(), serde_json::json!({}))
            .await
            .unwrap()
            .ok
    );
    assert_eq!(
        connection.effects.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
}

#[tokio::test]
async fn close_aborts_an_already_dispatched_call_without_reopening() {
    let connection = Arc::new(support::Connection {
        hold_call: true,
        ..support::Connection::default()
    });
    let (manager, _, connector, _) = support::manager(
        vec![support::config("docs")],
        vec![support::Plan::Client(connection.clone())],
    );
    let task = tokio::spawn({
        let manager = manager.clone();
        async move {
            manager
                .call_tool("mcp.docs.remote", model::JsonObject::new(), token())
                .await
        }
    });
    connection.call_started.notified().await;
    manager.close("docs").await.unwrap();
    assert_eq!(task.await.unwrap(), Err(McpClientError::Cancelled));
    assert_eq!(
        connector.attempts.load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    assert_eq!(
        connection.effects.load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    manager.dispose().await.unwrap();
}

#[tokio::test]
async fn connect_deadline_during_final_revision_check_closes_uninstalled_client() {
    let mut declaration = support::config("docs");
    declaration.resilience.max_connect_attempts = 1;
    declaration.resilience.connect_timeout_ms = 2;
    let connection = Arc::new(support::Connection::default());
    let (manager, secrets, _, _) = support::manager(
        vec![declaration],
        vec![support::Plan::Client(connection.clone())],
    );
    secrets
        .hold_generation_after
        .store(3, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        manager.connect("docs", token()).await,
        Err(McpClientError::Connect)
    );
    tokio::time::timeout(Duration::from_secs(1), async {
        while connection.closed.load(std::sync::atomic::Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(manager.connected_aliases().is_empty());
    manager.dispose().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn real_hung_stdio_initialize_is_killed_after_bounded_connect_timeout() {
    let directory = tempfile::tempdir().unwrap();
    let pid_file = directory.path().join("pid");
    let config = serde_json::from_value(serde_json::json!({"alias":"hung", "transport":{"kind":"stdio", "command":"bun",
        "args":[format!("{}/tests/fixtures/hang-stdio.mjs", env!("CARGO_MANIFEST_DIR"))], "env":{"PID_FILE":pid_file}},
        "resilience":{"maxConnectAttempts":1, "connectTimeoutMs":200}})).unwrap();
    let manager = McpClientManager::new(vec![config], Arc::new(Secrets::default())).unwrap();
    assert_eq!(
        manager.connect("hung", token()).await,
        Err(McpClientError::Connect)
    );
    let pid = std::fs::read_to_string(pid_file).expect("the real child started before the timeout");
    let gone = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let alive = std::process::Command::new("kill")
                .args(["-0", &pid])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap()
                .success();
            if !alive {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(gone.is_ok(), "timed-out stdio child remained alive");
    manager.dispose().await.unwrap();
}
