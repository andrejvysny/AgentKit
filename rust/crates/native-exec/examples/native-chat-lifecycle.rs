//! Standalone Claude Code backend lifecycle proof (AGENTKIT-118 / Page §8.1).
//!
//! Runs the *generic* native pipeline end to end — durable admission →
//! claim → preflight/plan → owned spawn → normalized events → authoritative
//! terminal outcome → durable session lineage — without the real `claude`
//! executable by supplying scripted in-process ports. The same wiring drives
//! the real driver (`agentkit_claude::default_backend()` /
//! `ClaudeCliBackend` + `ClaudeLocator`) when an official executable is
//! installed; only the ports differ.
//!
//! The scripted ports themselves are the workspace's single copy, in
//! `agentkit-testkit` (dev-only, ADR 0021 R10): what is proven here is the
//! lifecycle, not a private fake.
//!
//! ```text
//! cargo run -p agentkit-native-exec --example native-chat-lifecycle
//! ```

use agentkit_engine::{AgentHost, EngineResult, Runtime, SubmitTask, SystemRuntime, TaskExecutor};
use agentkit_native::{parse_target, validate_execution_target};
use agentkit_native_exec::{
    EXECUTION_TARGET_PAYLOAD_KEY, NativeAgentExecutor, NativeExecutorOptions,
    TURN_INPUT_PAYLOAD_KEY,
};
use agentkit_store::SqliteStore;
use agentkit_testkit::scripted::{
    ScriptedBackend, ScriptedLocator, ScriptedSupervisor, plain_chat_target, scripted_turn_input,
};
use serde_json::{Value, json};
use std::sync::Arc;

const SESSION: &str = "sess-example-1";

#[tokio::main]
async fn main() -> EngineResult<(), agentkit_store::StoreError> {
    let store = SqliteStore::memory(64).expect("in-memory store");
    let runtime: Arc<dyn Runtime> = Arc::new(SystemRuntime);

    let payload = json!({
        EXECUTION_TARGET_PAYLOAD_KEY: { "native": serde_json::to_value(plain_chat_target("example-config")).expect("serializable") },
        TURN_INPUT_PAYLOAD_KEY: serde_json::to_value(scripted_turn_input()).expect("serializable"),
    });

    store
        .submit(SubmitTask {
            task_id: "native-turn-1".into(),
            kind: "native.turn".into(),
            scope_id: "conversation-1".into(),
            payload,
            priority: 0,
            now: runtime.now_ms(),
            available_at: None,
            depends_on: vec![],
            parent_task_id: None,
            exclusive_scope: false,
        })
        .await
        .expect("admitted");

    let scripted = ScriptedSupervisor::default();
    scripted.emit(&json!({"e": "init", "s": SESSION}));
    scripted.emit(&json!({"e": "delta", "t": "Hello"}));
    scripted.emit(&json!({"e": "delta", "t": ", AgentKit."}));
    scripted.emit(&json!({"e": "result", "r": "Hello, AgentKit."}));

    let executor = Arc::new(NativeAgentExecutor::new(
        store.clone(),
        Arc::new(
            ScriptedBackend::new(SESSION)
                .with_run_dir("/tmp/agentkit-example-run")
                .with_fingerprint("example-fingerprint"),
        ),
        Arc::new(scripted.clone()),
        Arc::new(
            ScriptedLocator::default().with_evidence("sha256:example", "claude-pro (example)"),
        ),
        None,
        NativeExecutorOptions::default(),
    ));
    assert!(
        validate_execution_target(
            &parse_target(
                &store
                    .get_task("native-turn-1")
                    .await
                    .unwrap()
                    .expect("task")
                    .payload
            )
            .unwrap()
        )
        .is_ok()
    );

    // Durable dispatch: the same host loop any consumer runs — the executor
    // only ever sees a fenced ExecutionContext built by the runner.
    let mut executors = std::collections::HashMap::new();
    executors.insert(
        "native.turn".to_string(),
        executor as Arc<dyn TaskExecutor<SqliteStore>>,
    );
    let host = AgentHost::new(
        store.clone(),
        executors,
        runtime.clone(),
        agentkit_engine::HostOptions::default(),
    )?;
    // Fresh store: nothing to recover (manual-mode `recover` interrupts
    // queued tasks by design — it is a crash-recovery entry point, not a
    // pump). Drive the durable loop directly.
    for _ in 0..50 {
        if host.run_once().await.expect("pump") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    let events = store.events("native-turn-1", None, 1000).await.unwrap();
    for event in &events {
        let kind = event
            .event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("event");
        println!("event {:>3.0}: {kind}", event.seq);
    }
    let task = store.get_task("native-turn-1").await.unwrap().unwrap();
    println!(
        "task status: {:?} attempts={} error={:?}",
        task.status, task.attempt_count, task.error
    );
    assert_eq!(task.status, agentkit_store::TaskStatus::Completed);
    let binding = store
        .current_native_binding("native-turn-1")
        .await
        .unwrap()
        .expect("vendor session bound and observed");
    println!(
        "lineage: session={} state={} assigned_at={} observed_at={:?}",
        binding.vendor_session_id, binding.state, binding.assigned_at, binding.observed_at
    );
    assert_eq!(binding.vendor_session_id, SESSION);
    assert_eq!(binding.state, "observed");
    println!("standalone native lifecycle: OK");
    Ok(())
}
