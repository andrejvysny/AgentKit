use agentkit_host::*;
use agentkit_store::{SqliteStore, SubmitTask};
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
                }) as Arc<dyn VerificationHook>
            }),
            ..Default::default()
        };
        let host = AgentHost::new(
            store,
            HashMap::from([("fault".into(), Arc::new(executor) as Arc<dyn TaskExecutor>)]),
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
