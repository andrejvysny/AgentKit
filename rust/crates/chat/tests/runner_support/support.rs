// Shared driver support, compiled into every target that mounts it by path
// (`m0_fault_*`, `runner_conformance`); each target exercises a subset.
// Precedent: `tests/governance/support.rs`, `store/tests/support/mod.rs`.
#![allow(dead_code)]
use agentkit_chat::{AgentHost, ExecutionContext, HostOptions};
use agentkit_engine::{RetryBackoff, Runtime, TaskExecutor, VerificationHook};
use agentkit_store::{Lease, OutboxRecord, SqliteStore, SubmitTask};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::Notify;

pub struct Clock(pub AtomicI64);
impl Runtime for Clock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
    fn id(&self) -> String {
        uuid::Uuid::new_v4().to_string()
    }
}
pub struct Witness {
    pub effects: AtomicUsize,
    pub stale_denied: AtomicBool,
    pub verification: Mutex<Value>,
    pub captured: Mutex<Option<ExecutionContext>>,
    pub entered: Notify,
    pub release: Notify,
}
impl Witness {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            effects: AtomicUsize::new(0),
            stale_denied: AtomicBool::new(false),
            verification: Mutex::new(Value::Null),
            captured: Mutex::new(None),
            entered: Notify::new(),
            release: Notify::new(),
        })
    }
}
pub struct Environment {
    pub host: AgentHost,
    pub clock: Arc<Clock>,
    pub witness: Arc<Witness>,
}
impl Environment {
    pub async fn new(store: SqliteStore, scenario: &str, now: i64) -> Self {
        let witness = Witness::new();
        let clock = Arc::new(Clock(AtomicI64::new(now)));
        let executor = super::scenarios::Script {
            scenario: scenario.into(),
            witness: witness.clone(),
        };
        let options = HostOptions {
            lease_ttl_ms: 1000,
            poll_interval_ms: 2,
            concurrency: 1,
            retry_backoff: RetryBackoff {
                base_ms: 200,
                max_ms: 2000,
                jitter_ratio: 0.0,
            },
            verification: (scenario == "verification").then(|| {
                Arc::new(super::scenarios::Verify {
                    store: store.clone(),
                    witness: witness.clone(),
                }) as Arc<dyn VerificationHook<agentkit_store::SqliteStore>>
            }),
            ..Default::default()
        };
        let host = AgentHost::new(
            store,
            HashMap::from([(
                "fault".into(),
                Arc::new(executor) as Arc<dyn TaskExecutor<agentkit_store::SqliteStore>>,
            )]),
            clock.clone(),
            options,
        )
        .unwrap();
        host.recover().await.unwrap();
        Self {
            host,
            clock,
            witness,
        }
    }
    pub async fn submit(&self) {
        self.host
            .submit(SubmitTask {
                task_id: "run".into(),
                kind: "fault".into(),
                scope_id: "scope".into(),
                payload: json!({}),
                priority: 0,
                now: 1000,
                available_at: None,
                depends_on: vec![],
                parent_task_id: None,
                exclusive_scope: false,
            })
            .await
            .unwrap();
    }
    pub fn at(&self, now: i64) {
        self.clock.0.store(now, Ordering::SeqCst);
    }
}

pub async fn entered(env: &Environment) {
    tokio::time::timeout(Duration::from_secs(10), env.witness.entered.notified())
        .await
        .expect("fault worker did not reach gate");
}

pub async fn snapshot(env: &Environment, label: &str, extra: Value) -> Value {
    let store = env.host.store();
    let task = store.get_task("run").await.unwrap().unwrap();
    let attempts = store.attempts("run").await.unwrap();
    let events = store.events("run", None, 100).await.unwrap();
    for event in &events {
        assert_eq!(event.event["contractVersion"], "0.7.0");
        assert!(event.event["eventId"].is_string());
        assert!(event.event["attemptId"].is_string());
        chrono::DateTime::parse_from_rfc3339(event.event["timestamp"].as_str().unwrap()).unwrap();
    }
    let failure = (task.status == agentkit_store::TaskStatus::Failed)
        .then(|| {
            task.error
                .as_ref()
                .map(|error| error.split(':').next().unwrap().to_owned())
        })
        .flatten();
    let mut value = json!({
        "label":label,"status":task.status,"attemptCount":task.attempt_count,
        "attempts":attempts.into_iter().map(|item|item.status).collect::<Vec<_>>(),
        "effects":env.witness.effects.load(Ordering::SeqCst),
        "events":events.into_iter().map(|item|json!({"seq":item.event["seq"],"type":item.event["type"],"data":item.event["data"]})).collect::<Vec<_>>(),
        "poisonCount":task.poison_count,"deadLetter":task.dead_lettered_at.is_some(),"failureReason":failure,
    });
    value
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    value
}

/// One profile of the shared fault schedule. The field names mirror
/// `runner-faults-v1.json` exactly, so the same document drives the
/// six-runner differential gate and the M0 boundary tests.
#[derive(Deserialize)]
pub struct Profile {
    pub name: String,
    #[serde(default)]
    pub schedule: Vec<String>,
    #[serde(default)]
    pub delta: Option<String>,
    pub expected: Expected,
}

#[derive(Deserialize)]
pub struct Expected {
    /// The TypeScript oracle's checkpoints. The M0 profiles are executed by
    /// the Rust host only, so they carry an empty list here — see
    /// `runner-faults.md`, "M0 boundary profiles".
    #[serde(default)]
    pub typescript: Vec<Value>,
    pub rust: Vec<Value>,
}

#[derive(Deserialize)]
pub struct Schedule {
    pub scenarios: Vec<Profile>,
}

/// The single shared fault schedule the M0 gate is authored against.
pub fn schedule() -> Schedule {
    serde_json::from_str(include_str!(
        "../../../contracts/tests/fixtures/conformance/runner-faults-v1.json"
    ))
    .expect("the shared fault schedule parses")
}

/// A profile plus its documented schedule, so a missing profile is a test
/// failure rather than a silently skipped boundary.
pub fn profile(name: &str) -> Profile {
    schedule()
        .scenarios
        .into_iter()
        .find(|scenario| scenario.name == name)
        .unwrap_or_else(|| panic!("profile {name} is missing from the shared schedule"))
}

/// The live attempt lease of a suspended run: the fencing proof the driver
/// uses to stand in for the dispatch layer at a receipt boundary.
pub async fn lease(env: &Environment, task_id: &str) -> Lease {
    env.host
        .store()
        .get_lease(task_id)
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("task {task_id} holds no attempt lease"))
}

/// The client-visible publication rows for one run: every event payload the
/// store queued for delivery, in insertion order.
pub async fn publication(env: &Environment, run_id: &str) -> Vec<OutboxRecord> {
    env.host
        .store()
        .outbox()
        .await
        .unwrap()
        .into_iter()
        .filter(|record| record.topic == "task.events" && record.run_id.as_deref() == Some(run_id))
        .collect()
}

pub struct Directory(pub PathBuf);
impl Directory {
    pub fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("agentkit-runner-vectors-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
