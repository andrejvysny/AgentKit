#![allow(dead_code)]
use agentkit_core::{ToolContext, ToolLimits};
use agentkit_mcp::*;
use async_trait::async_trait;
use serde_json::json;
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Barrier, Notify};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
pub struct Secrets {
    pub values: Mutex<BTreeMap<String, String>>,
    pub revision: AtomicU64,
    pub generation_calls: AtomicUsize,
    pub hold_generation_after: AtomicUsize,
}
#[async_trait]
impl McpSecretResolver for Secrets {
    async fn get(&self, reference: &str) -> Result<Option<String>, McpClientError> {
        Ok(self.values.lock().unwrap().get(reference).cloned())
    }
    async fn generation(&self, _: &BTreeMap<String, String>) -> Result<u64, McpClientError> {
        let count = self.generation_calls.fetch_add(1, Ordering::SeqCst) + 1;
        let hold = self.hold_generation_after.load(Ordering::SeqCst);
        if hold > 0 && count >= hold {
            std::future::pending::<()>().await;
        }
        Ok(self.revision.load(Ordering::SeqCst))
    }
}
#[derive(Default)]
pub struct Clock {
    pub now: AtomicU64,
    pub sleeps: Mutex<Vec<u64>>,
}
#[async_trait]
impl ManagerClock for Clock {
    fn now_ms(&self) -> u64 {
        self.now.load(Ordering::SeqCst)
    }
    async fn sleep(
        &self,
        duration: Duration,
        cancellation: CancellationToken,
    ) -> Result<(), McpClientError> {
        if cancellation.is_cancelled() {
            return Err(McpClientError::Cancelled);
        }
        let milliseconds = duration.as_millis() as u64;
        self.sleeps.lock().unwrap().push(milliseconds);
        self.now.fetch_add(milliseconds, Ordering::SeqCst);
        Ok(())
    }
}
#[derive(Default)]
pub struct Connection {
    pub lists: Mutex<VecDeque<Result<Vec<model::Tool>, McpClientError>>>,
    pub calls: Mutex<VecDeque<Result<model::CallToolResult, McpClientError>>>,
    pub effects: AtomicUsize,
    pub closed: AtomicUsize,
    pub names: Mutex<Vec<String>>,
    pub barrier: Option<Arc<Barrier>>,
    pub call_started: Notify,
    pub hold_call: bool,
    pub reject_before_dispatch: bool,
}
pub fn tools() -> Vec<model::Tool> {
    vec![serde_json::from_value(json!({"name":"remote", "inputSchema":{"type":"object"}, "annotations":{"readOnlyHint":true}})).unwrap()]
}
#[async_trait]
impl McpConnection for Connection {
    async fn list_tools(&self, _: CancellationToken) -> Result<Vec<model::Tool>, McpClientError> {
        if let Some(barrier) = &self.barrier {
            barrier.wait().await;
        }
        self.lists
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Ok(tools()))
    }
    async fn call_tool(
        &self,
        name: &str,
        _: model::JsonObject,
        _: CancellationToken,
    ) -> Result<model::CallToolResult, McpClientError> {
        if self.reject_before_dispatch {
            return Err(McpClientError::Closed);
        }
        self.effects.fetch_add(1, Ordering::SeqCst);
        self.names.lock().unwrap().push(name.into());
        self.call_started.notify_one();
        if self.hold_call {
            std::future::pending::<()>().await;
        }
        self.calls.lock().unwrap().pop_front().unwrap_or_else(|| {
            Ok(model::CallToolResult::success(vec![
                model::ContentBlock::text("ok"),
            ]))
        })
    }
    async fn close(&self) -> Result<(), McpClientError> {
        self.closed.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}
pub enum Plan {
    Fail(McpClientError),
    Client(Arc<Connection>),
    Hold,
}
#[derive(Default)]
pub struct Connector {
    pub plans: Mutex<VecDeque<Plan>>,
    pub transports: Mutex<Vec<TransportConfig>>,
    pub attempts: AtomicUsize,
    pub started: Notify,
    pub dropped: Arc<AtomicUsize>,
}
struct Dropped(Arc<AtomicUsize>);
impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
#[async_trait]
impl McpConnector for Connector {
    async fn connect(
        &self,
        transport: TransportConfig,
        _: ClientOptions,
        _: CancellationToken,
    ) -> Result<Arc<dyn McpConnection>, McpClientError> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        self.transports.lock().unwrap().push(transport);
        self.started.notify_one();
        let plan = self
            .plans
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected connection attempt");
        match plan {
            Plan::Fail(error) => Err(error),
            Plan::Client(client) => Ok(client),
            Plan::Hold => {
                let _drop = Dropped(self.dropped.clone());
                std::future::pending().await
            }
        }
    }
}
pub fn config(alias: &str) -> ServerConfig {
    serde_json::from_value(json!({"alias":alias,"transport":{"kind":"stdio","command":"server","env":{"TOKEN":"${token}"}}})).unwrap()
}
pub fn manager(
    configs: Vec<ServerConfig>,
    plans: Vec<Plan>,
) -> (
    Arc<McpClientManager>,
    Arc<Secrets>,
    Arc<Connector>,
    Arc<Clock>,
) {
    let (secrets, connector, clock) = (
        Arc::new(Secrets::default()),
        Arc::new(Connector::default()),
        Arc::new(Clock::default()),
    );
    connector.plans.lock().unwrap().extend(plans);
    let manager =
        McpClientManager::with_ports(configs, secrets.clone(), connector.clone(), clock.clone())
            .unwrap();
    (Arc::new(manager), secrets, connector, clock)
}
pub fn context() -> ToolContext {
    ToolContext {
        suspension: None,
        run_id: "run".into(),
        tool_call_id: None,
        attempt_id: None,
        chat_id: Some("chat".into()),
        scope_id: None,
        user_id: None,
        bindings: vec![],
        limits: ToolLimits::default(),
        cancellation: CancellationToken::new(),
        metadata: serde_json::Map::new(),
        dispatch_guard: None,
    }
}
pub fn token() -> CancellationToken {
    CancellationToken::new()
}
