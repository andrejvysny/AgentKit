//! The open builder: a consumer-registered task kind is served by the turn
//! executor, and with a native lane configured, a task whose frozen target is
//! native runs the durable native path through the scripted ports
//! (AGENTKIT-107 §8 / phase C1). A runtime with NO provider at all builds
//! when a native lane — or the consumer's own executors — owns every turn
//! kind, and a turn that then still needs the model loop settles failed with
//! the typed `no_model_provider` refusal instead of running (ADR 0026).
//!
//! The scripted ports come from `agentkit-testkit`; this file keeps only the
//! scenario, never a second copy of the fakes.

use std::sync::Arc;

use agentkit_native::{EXECUTION_TARGET_PAYLOAD_KEY, TURN_INPUT_PAYLOAD_KEY};
use agentkit_native_exec::NativeExecutorOptions;
use agentkit_runtime::{AgentRuntime, NativeLane, RefusingExecutor};
use agentkit_store::{SqliteStore, TaskRecord, TaskStatus};
use agentkit_testkit::scripted::{
    ScriptedBackend, ScriptedChatProvider, ScriptedLocator, ScriptedSupervisor, plain_chat_target,
    scripted_turn_input,
};
use async_trait::async_trait;
use serde_json::{Value, json};

const SESSION: &str = "sess-native-lane-1";
const KIND: &str = "consumer.chat";

/// The native lane of these scenarios: a scripted PlainChat launch of the
/// scripted child, with no driver crate and no vendor executable anywhere.
fn lane(backend: Arc<ScriptedBackend>, supervisor: ScriptedSupervisor) -> NativeLane {
    NativeLane {
        backend,
        supervisor: Arc::new(supervisor),
        locator: Arc::new(
            ScriptedLocator::default().with_evidence("sha256:test", "claude-pro (test)"),
        ),
        broker_factory: None,
        options: NativeExecutorOptions::default(),
    }
}

fn native_payload() -> Value {
    json!({
        EXECUTION_TARGET_PAYLOAD_KEY: { "native": serde_json::to_value(plain_chat_target("test-config")).expect("serializable") },
        TURN_INPUT_PAYLOAD_KEY: serde_json::to_value(scripted_turn_input()).expect("serializable"),
    })
}

async fn admit(runtime: &AgentRuntime, task_id: &str, kind: &str, payload: Value) {
    runtime
        .host()
        .submit(agentkit_engine::SubmitTask {
            task_id: task_id.into(),
            kind: kind.into(),
            scope_id: "conversation-1".into(),
            payload,
            priority: 0,
            now: 1,
            available_at: None,
            depends_on: vec![],
            parent_task_id: None,
            exclusive_scope: false,
        })
        .await
        .expect("admitted under the consumer kind");
}

/// Deterministic driving: the caller has parked the background worker, so the
/// only execution is the pump below.
async fn settle(runtime: &AgentRuntime, store: &SqliteStore, task_id: &str) -> TaskRecord {
    for _ in 0..50 {
        runtime.host().run_once().await.expect("pump");
        let task = store.get_task(task_id).await.unwrap().expect("task exists");
        if matches!(task.status, TaskStatus::Completed | TaskStatus::Failed) {
            return task;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("task {task_id} never settled");
}

async fn park_worker(runtime: &mut AgentRuntime) {
    if let Some(worker) = runtime.take_worker() {
        worker.shutdown().await.expect("shutdown worker");
    }
}

/// A scripted child that greets, exactly as the standalone lifecycle proof.
fn greeting() -> (ScriptedSupervisor, ScriptedBackend) {
    let supervisor = ScriptedSupervisor::default();
    supervisor.emit(&json!({"e": "init", "s": SESSION}));
    supervisor.emit(&json!({"e": "delta", "t": "Hello"}));
    supervisor.emit(&json!({"e": "delta", "t": ", AgentKit."}));
    supervisor.emit(&json!({"e": "result", "r": "Hello, AgentKit."}));
    let backend = ScriptedBackend::new(SESSION)
        .with_run_dir("/tmp/agentkit-native-lane-test")
        .with_fingerprint("native-lane-test-fingerprint");
    (supervisor, backend)
}

#[tokio::test(flavor = "multi_thread")]
async fn consumer_kind_with_native_lane_runs_the_scripted_backend() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteStore::open(dir.path().join("store.sqlite"), 8).expect("open store");
    let (supervisor, backend) = greeting();
    let backend = Arc::new(backend);

    let mut runtime = AgentRuntime::builder(store.clone())
        .default_provider(Arc::new(ScriptedChatProvider::completing(
            "fake-native-lane",
            "ok",
        )))
        .turn_kinds(vec![KIND.into()])
        .native_lane(lane(Arc::clone(&backend), supervisor))
        .build()
        .await
        .expect("build runtime with native lane");
    park_worker(&mut runtime).await;

    admit(&runtime, "native-turn-1", KIND, native_payload()).await;
    let task = settle(&runtime, &store, "native-turn-1").await;
    assert_eq!(task.status, TaskStatus::Completed);
    assert!(
        backend.planned(),
        "the scripted backend must have been called"
    );
    let binding = store
        .current_native_binding("native-turn-1")
        .await
        .unwrap()
        .expect("vendor session bound");
    assert_eq!(binding.vendor_session_id, SESSION);
}

/// The providerless composition root (F3): with a native lane configured the
/// builder needs no model provider at all, and a native-target turn still
/// runs the durable native path end to end.
#[tokio::test(flavor = "multi_thread")]
async fn providerless_build_runs_a_native_target_turn() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteStore::open(dir.path().join("store.sqlite"), 8).expect("open store");
    let (supervisor, backend) = greeting();
    let backend = Arc::new(backend);

    let mut runtime = AgentRuntime::builder(store.clone())
        .turn_kinds(vec![KIND.into()])
        .native_lane(lane(Arc::clone(&backend), supervisor))
        .build()
        .await
        .expect("a native lane makes the provider optional");
    park_worker(&mut runtime).await;

    admit(&runtime, "native-turn-1", KIND, native_payload()).await;
    let task = settle(&runtime, &store, "native-turn-1").await;
    assert_eq!(task.status, TaskStatus::Completed);
    assert!(backend.planned(), "the native path must have run");
    let binding = store
        .current_native_binding("native-turn-1")
        .await
        .unwrap()
        .expect("vendor session bound");
    assert_eq!(binding.vendor_session_id, SESSION);
}

/// What the ADR 0026 open question turned on: a providerless runtime may not
/// be billed for a model loop it does not have. A payload with no frozen
/// native target falls through the dispatch wrapper onto the refusal, which
/// settles it failed with a deterministic code — never a panic.
#[tokio::test(flavor = "multi_thread")]
async fn providerless_native_lane_refuses_a_model_loop_turn() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteStore::open(dir.path().join("store.sqlite"), 8).expect("open store");
    let (supervisor, backend) = greeting();

    let mut runtime = AgentRuntime::builder(store.clone())
        .turn_kinds(vec![KIND.into()])
        .native_lane(lane(Arc::new(backend), supervisor))
        .build()
        .await
        .expect("a native lane makes the provider optional");
    park_worker(&mut runtime).await;

    admit(
        &runtime,
        "model-turn-1",
        KIND,
        json!({"marker": "no frozen target"}),
    )
    .await;
    let task = settle(&runtime, &store, "model-turn-1").await;
    assert_eq!(task.status, TaskStatus::Failed);
    let error = task.error.expect("the refusal is durable");
    assert!(
        error.contains(RefusingExecutor::NO_MODEL_PROVIDER),
        "the refusal must name its code: {error}"
    );
}

/// No provider, no native lane, and no consumer owner for the turn kinds: the
/// missing model loop is refused at build time, by name.
#[tokio::test(flavor = "multi_thread")]
async fn providerless_build_without_a_lane_or_turn_executor_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteStore::open(dir.path().join("store.sqlite"), 8).expect("open store");
    let error = match AgentRuntime::builder(store)
        .turn_kinds(vec![KIND.into()])
        .build()
        .await
    {
        Ok(_) => panic!("a runtime with no model loop and no owner was built"),
        Err(error) => error,
    };
    let message = error.to_string();
    assert!(
        message.contains("a default provider is required"),
        "unexpected error: {message}"
    );
    for missing in ["chat.turn", KIND] {
        assert!(
            message.contains(missing),
            "the refusal must name the unowned kind {missing}: {message}"
        );
    }
}

/// A consumer that admits no chat turns at all takes the turn kinds over: the
/// builder's refusal stand-in is exactly what it is entitled to replace.
#[tokio::test(flavor = "multi_thread")]
async fn providerless_build_lets_a_consumer_own_every_turn_kind() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteStore::open(dir.path().join("store.sqlite"), 8).expect("open store");
    let mut runtime = AgentRuntime::builder(store.clone())
        .turn_kinds(vec![KIND.into()])
        .executor(KIND, Arc::new(NoopExecutor))
        .executor("chat.turn", Arc::new(RefusingExecutor::no_model_provider()))
        .build()
        .await
        .expect("the consumer owns every turn kind");
    park_worker(&mut runtime).await;

    admit(&runtime, "consumer-turn-1", KIND, json!({"marker": "mine"})).await;
    let task = settle(&runtime, &store, "consumer-turn-1").await;
    assert_eq!(task.status, TaskStatus::Completed);

    // The takeover is one-way per kind: registering a kind twice over stays
    // a build-time refusal even on the providerless path.
    let error = match AgentRuntime::builder(store)
        .executor(KIND, Arc::new(NoopExecutor))
        .executor(KIND, Arc::new(NoopExecutor))
        .executor("chat.turn", Arc::new(NoopExecutor))
        .build()
        .await
    {
        Ok(_) => panic!("one kind, one owner"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("registered twice"),
        "unexpected error: {error}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn registering_an_executor_for_an_owned_kind_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteStore::open(dir.path().join("store.sqlite"), 8).expect("open store");
    let duplicate: Arc<dyn agentkit_engine::TaskExecutor<SqliteStore>> = Arc::new(NoopExecutor);
    let error = match AgentRuntime::builder(store)
        .default_provider(Arc::new(ScriptedChatProvider::completing("fake-dup", "ok")))
        .executor("chat.turn", duplicate)
        .build()
        .await
    {
        Ok(_) => panic!("chat.turn is owned by the runtime"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("registered twice"),
        "unexpected error: {error}"
    );
}

struct NoopExecutor;

#[async_trait]
impl agentkit_engine::TaskExecutor<SqliteStore> for NoopExecutor {
    async fn execute(
        &self,
        _context: agentkit_engine::ExecutionContext<SqliteStore>,
    ) -> agentkit_engine::EngineResult<(), agentkit_store::StoreError> {
        Ok(())
    }
}
