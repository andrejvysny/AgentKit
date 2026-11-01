//! Registration/generation conformance for the MCP tool-handle lifecycle, the
//! alias→effective identity contract, and configuration replacement.
//!
//! Every test here is a regression guard for one claim: the callback that RUNS
//! is always the callback that was AUTHORIZED. The witness is always the
//! transport itself (a stub recording what it actually received, or a real
//! loopback MCP server counting requests), never an internal counter.
#[path = "support/managed.rs"]
mod support;
use agentkit_core::{CoreError, DispatchFence, DispatchGuard, ToolContext, ToolLimits};
use agentkit_mcp::*;
use async_trait::async_trait;
use serde_json::{Map, json};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
use support::{Clock, Secrets, token};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

/// One transport generation. The payload names the generation, so "which
/// callback answered" is readable straight out of the result, and `received`
/// is the wire witness: a name appears there only if the call was dispatched.
struct Stub {
    payload: String,
    advertised: String,
    received: Mutex<Vec<String>>,
    closes: AtomicUsize,
    fail_close: AtomicBool,
}

impl Stub {
    fn new(payload: &str) -> Arc<Self> {
        Arc::new(Self {
            payload: payload.into(),
            advertised: "t".into(),
            received: Mutex::new(Vec::new()),
            closes: AtomicUsize::new(0),
            fail_close: AtomicBool::new(false),
        })
    }
    fn advertising(mut self: Arc<Self>, name: &str) -> Arc<Self> {
        Arc::get_mut(&mut self).expect("fresh stub").advertised = name.into();
        self
    }
    fn failing_close(self: &Arc<Self>) -> Arc<Self> {
        self.fail_close.store(true, Ordering::SeqCst);
        Arc::clone(self)
    }
    fn received(&self) -> Vec<String> {
        self.received.lock().unwrap().clone()
    }
}

#[async_trait]
impl McpConnection for Stub {
    async fn list_tools(&self, _: CancellationToken) -> Result<Vec<model::Tool>, McpClientError> {
        Ok(vec![
            serde_json::from_value(
                json!({"name": self.advertised, "inputSchema": {"type":"object"}}),
            )
            .expect("a legal tool shape"),
        ])
    }
    async fn call_tool(
        &self,
        name: &str,
        _: model::JsonObject,
        _: CancellationToken,
    ) -> Result<model::CallToolResult, McpClientError> {
        self.received.lock().unwrap().push(name.into());
        Ok(model::CallToolResult::success(vec![
            model::ContentBlock::text(self.payload.clone()),
        ]))
    }
    async fn close(&self) -> Result<(), McpClientError> {
        self.closes.fetch_add(1, Ordering::SeqCst);
        if self.fail_close.load(Ordering::SeqCst) {
            return Err(McpClientError::Disconnected);
        }
        Ok(())
    }
}

/// Hands out prepared stubs in order; the transport declaration never matters,
/// so no test here depends on a real socket or child process.
#[derive(Default)]
struct Stubs {
    queue: Mutex<VecDeque<Arc<dyn McpConnection>>>,
    attempts: AtomicUsize,
}

#[async_trait]
impl McpConnector for Stubs {
    async fn connect(
        &self,
        _: TransportConfig,
        _: ClientOptions,
        _: CancellationToken,
    ) -> Result<Arc<dyn McpConnection>, McpClientError> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        self.queue
            .lock()
            .unwrap()
            .pop_front()
            .ok_or(McpClientError::Connect)
    }
}

fn manager_with(
    configs: Vec<ServerConfig>,
    stubs: Vec<Arc<Stub>>,
) -> (Arc<McpClientManager>, Arc<Stubs>) {
    let connector = Arc::new(Stubs::default());
    connector
        .queue
        .lock()
        .unwrap()
        .extend(stubs.into_iter().map(|stub| stub as Arc<dyn McpConnection>));
    let manager = McpClientManager::with_ports(
        configs,
        Arc::new(Secrets::default()),
        connector.clone(),
        Arc::new(Clock::default()),
    )
    .expect("test configs are valid");
    (Arc::new(manager), connector)
}

fn config_with(alias: &str, aliases: &[(&str, &str)]) -> ServerConfig {
    ServerConfig {
        alias: alias.into(),
        transport: TransportConfig::Stdio {
            command: "server".into(),
            args: vec![],
            env: BTreeMap::new(),
        },
        secret_refs: BTreeMap::new(),
        enabled: true,
        tool_aliases: aliases
            .iter()
            .map(|(source, target)| ((*source).into(), (*target).into()))
            .collect(),
        read_only: false,
        resilience: Resilience::default(),
    }
}

fn context() -> ToolContext {
    context_with(CancellationToken::new(), Arc::new(support::Allow))
}

fn context_with(cancellation: CancellationToken, guard: Arc<dyn DispatchGuard>) -> ToolContext {
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
        cancellation,
        metadata: Map::new(),
        dispatch_guard: Some(guard),
    }
}

fn error_code(result: &agentkit_core::ToolResult) -> String {
    result.model_data.as_ref().unwrap()["errorCode"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

fn text(result: &agentkit_core::ToolResult) -> String {
    result.model_data.as_ref().unwrap()["text"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

/// A guard whose verdict can be flipped, and whose `check` can be held open so
/// a test can change the world in the middle of the authorization await.
#[derive(Debug)]
struct Gate {
    entered: Notify,
    release: Notify,
    verdict: AtomicBool,
    checks: AtomicUsize,
}

impl Gate {
    fn allowing() -> Arc<Self> {
        Arc::new(Self {
            entered: Notify::new(),
            release: Notify::new(),
            verdict: AtomicBool::new(true),
            checks: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl DispatchGuard for Gate {
    async fn check(&self) -> Result<(), CoreError> {
        let call = self.checks.fetch_add(1, Ordering::SeqCst);
        if call == 0 {
            self.entered.notify_one();
            self.release.notified().await;
        }
        if self.verdict.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err(CoreError::new("denied", "refused by the test guard"))
        }
    }
    fn fence(&self) -> DispatchFence {
        DispatchFence {
            task_id: "task".into(),
            attempt_id: "attempt".into(),
            fencing_token: 1,
        }
    }
}

/// A guard that reports "authorized" and, on the way out, cancels the run —
/// the exact shape of a settlement that lands after the authorization await
/// but before the transport write.
#[derive(Debug)]
struct CancelOnCheck {
    cancellation: CancellationToken,
}

#[async_trait]
impl DispatchGuard for CancelOnCheck {
    async fn check(&self) -> Result<(), CoreError> {
        self.cancellation.cancel();
        Ok(())
    }
    fn fence(&self) -> DispatchFence {
        DispatchFence {
            task_id: "task".into(),
            attempt_id: "attempt".into(),
            fencing_token: 1,
        }
    }
}

/// A host authority that always denies. The bound dispatch path consults the
/// host guard exactly where it dispatches, so a denial there must stop the
/// call from reaching the server.
#[derive(Debug, Default)]
struct DenyGuard;

#[async_trait]
impl DispatchGuard for DenyGuard {
    async fn check(&self) -> Result<(), CoreError> {
        Err(CoreError::new("revoked", "authority revoked"))
    }
    fn fence(&self) -> DispatchFence {
        DispatchFence {
            task_id: "task".into(),
            attempt_id: "attempt".into(),
            fencing_token: 1,
        }
    }
}

/// A host authority that allows and counts its consultations: one dispatch
/// attempt must consult it exactly once — the decision that authorizes the
/// call is the one taken immediately before the wire call, neither bypassed
/// nor duplicated (a human approval gate charged twice per call would be a
/// defect of its own).
#[derive(Debug, Default)]
struct CountingGuard {
    checks: AtomicUsize,
}

#[async_trait]
impl DispatchGuard for CountingGuard {
    async fn check(&self) -> Result<(), CoreError> {
        self.checks.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn fence(&self) -> DispatchFence {
        DispatchFence {
            task_id: "task".into(),
            attempt_id: "attempt".into(),
            fencing_token: 1,
        }
    }
}

#[tokio::test]
async fn a_retained_handle_never_executes_its_replacements_callback() {
    let first = Stub::new("v1");
    let second = Stub::new("v2");
    let (manager, _) = manager_with(
        vec![config_with("a", &[])],
        vec![first.clone(), second.clone()],
    );
    let handle = manager.contribute(token()).await.unwrap().tools.remove(0);

    let authorized = handle.execute(context(), json!({})).await.unwrap();
    assert!(authorized.ok);
    assert_eq!(text(&authorized), "v1");
    assert_eq!(first.received(), ["t"]);

    // Same alias, same tool name, different server behind it.
    let mut replacement = config_with("a", &[]);
    replacement.transport = TransportConfig::Http {
        url: "http://127.0.0.1:9/mcp".into(),
        headers: BTreeMap::new(),
    };
    manager.replace_configs(vec![replacement]).await.unwrap();

    let stale = handle.execute(context(), json!({})).await.unwrap();
    assert!(!stale.ok);
    assert_eq!(error_code(&stale), "mcp_not_connected");
    assert!(
        second.received().is_empty(),
        "the replacement callback ran for a descriptor authorized against the previous one"
    );
    assert_eq!(
        first.received(),
        ["t"],
        "a revoked handle must not re-dispatch"
    );

    // ...and the new generation's own descriptor reaches only the new callback.
    let fresh = manager.contribute(token()).await.unwrap().tools.remove(0);
    let current = fresh.execute(context(), json!({})).await.unwrap();
    assert!(current.ok);
    assert_eq!(text(&current), "v2");
    assert_eq!(second.received(), ["t"]);
    assert_eq!(first.received(), ["t"]);
    manager.dispose().await.unwrap();
}

#[tokio::test]
async fn a_reconnect_under_one_alias_refuses_the_previous_generations_handle() {
    let first = Stub::new("generation-1");
    let second = Stub::new("generation-2");
    let (manager, connector) = manager_with(
        vec![config_with("a", &[])],
        vec![first.clone(), second.clone()],
    );
    let handle = manager.contribute(token()).await.unwrap().tools.remove(0);
    assert_eq!(
        text(&handle.execute(context(), json!({})).await.unwrap()),
        "generation-1"
    );

    // The config never changed; only the transport did. A second server process
    // behind the same alias is still a different callback.
    manager.close("a").await.unwrap();
    manager.connect("a", token()).await.unwrap();

    let stale = handle.execute(context(), json!({})).await.unwrap();
    assert!(!stale.ok);
    assert_eq!(error_code(&stale), "mcp_authority_revoked");
    assert!(
        second.received().is_empty(),
        "a handle minted from transport generation 1 dispatched on generation 2"
    );

    let fresh = manager.contribute(token()).await.unwrap().tools.remove(0);
    assert_eq!(
        text(&fresh.execute(context(), json!({})).await.unwrap()),
        "generation-2"
    );
    assert_eq!(second.received(), ["t"]);
    assert_eq!(connector.attempts.load(Ordering::SeqCst), 2);
    manager.dispose().await.unwrap();
}

#[tokio::test]
async fn a_replacement_landing_inside_the_authorization_await_is_not_executed() {
    let first = Stub::new("v1");
    let second = Stub::new("v2");
    let (manager, _) = manager_with(
        vec![config_with("a", &[])],
        vec![first.clone(), second.clone()],
    );
    let handle = manager.contribute(token()).await.unwrap().tools.remove(0);
    // The handle is live before the swap: one authorized call already landed.
    assert_eq!(
        text(&handle.execute(context(), json!({})).await.unwrap()),
        "v1"
    );
    let gate = Gate::allowing();
    let executing = {
        let handle = Arc::clone(&handle);
        let guard: Arc<dyn DispatchGuard> = gate.clone();
        tokio::spawn(async move {
            handle
                .execute(context_with(token(), guard), json!({}))
                .await
                .expect("the stub transport never fails the bridge")
        })
    };
    gate.entered.notified().await;
    // The host has said "yes" and is still awaiting; the server is swapped out
    // under the handle in that window.
    let mut replacement = config_with("a", &[]);
    replacement.transport = TransportConfig::Http {
        url: "http://127.0.0.1:9/mcp".into(),
        headers: BTreeMap::new(),
    };
    manager.replace_configs(vec![replacement]).await.unwrap();
    gate.release.notify_waiters();

    let result = executing.await.unwrap();
    assert!(!result.ok);
    assert!(
        ["mcp_not_connected", "mcp_authority_revoked"].contains(&error_code(&result).as_str()),
        "unexpected failure class: {}",
        error_code(&result)
    );
    assert!(second.received().is_empty());
    assert_eq!(first.received(), ["t"], "the call was dispatched anyway");
    manager.dispose().await.unwrap();
}

#[tokio::test]
async fn one_effective_name_across_two_servers_routes_each_identity_to_its_own_callback() {
    let plain = Stub::new("server-one").advertising("t");
    let renamed = Stub::new("server-two").advertising("x");
    let (manager, _) = manager_with(
        vec![config_with("one", &[]), config_with("two", &[("x", "t")])],
        vec![plain.clone(), renamed.clone()],
    );
    let report = manager.contribute(token()).await.unwrap();
    let mut capabilities: Vec<String> = report
        .tools
        .iter()
        .map(|tool| tool.definition().capability.clone())
        .collect();
    capabilities.sort();
    assert_eq!(capabilities, ["mcp.one.t", "mcp.two.t"]);

    // `mcp.two.t` is server two's renamed `x`; the reverse map must not be able
    // to answer it with server one's callback, nor vice versa.
    manager
        .call_tool("mcp.two.t", model::JsonObject::new(), token())
        .await
        .unwrap();
    assert_eq!(renamed.received(), ["x"]);
    assert!(plain.received().is_empty());
    manager
        .call_tool("mcp.one.t", model::JsonObject::new(), token())
        .await
        .unwrap();
    assert_eq!(plain.received(), ["t"]);
    assert_eq!(renamed.received(), ["x"]);

    // The contributed handles agree with the canonical route, tool for tool.
    for tool in &report.tools {
        let result = tool.execute(context(), json!({})).await.unwrap();
        assert!(result.ok);
        assert_eq!(
            result.data.unwrap()["serverAlias"].as_str().unwrap(),
            if tool.definition().capability == "mcp.one.t" {
                "one"
            } else {
                "two"
            }
        );
    }
    assert_eq!(plain.received(), ["t", "t"]);
    assert_eq!(renamed.received(), ["x", "x"]);
    manager.dispose().await.unwrap();
}

#[tokio::test]
async fn a_renamed_tool_is_not_addressable_by_the_name_it_left_behind() {
    let stub = Stub::new("only").advertising("remote");
    let (manager, _) = manager_with(
        vec![config_with("docs", &[("remote", "search.query")])],
        vec![stub.clone()],
    );
    let report = manager.contribute(token()).await.unwrap();
    assert_eq!(
        report.tools[0].definition().capability,
        "mcp.docs.search.query"
    );
    report.tools[0].execute(context(), json!({})).await.unwrap();
    assert_eq!(stub.received(), ["remote"]);

    // `mcp.docs.remote` is not this server's address: the config renamed that
    // name away, so dispatch refuses instead of quietly reaching the tool whose
    // canonical identity is `mcp.docs.search.query`.
    assert_eq!(
        manager
            .call_tool("mcp.docs.remote", model::JsonObject::new(), token())
            .await,
        Err(McpClientError::Configuration)
    );
    assert_eq!(stub.received(), ["remote"], "a refused address dispatched");

    manager
        .call_tool("mcp.docs.search.query", model::JsonObject::new(), token())
        .await
        .unwrap();
    assert_eq!(stub.received(), ["remote", "remote"]);
    manager.dispose().await.unwrap();
}

#[tokio::test]
async fn a_chained_rename_is_refused_at_construction_by_both_gates() {
    let stub = Stub::new("chained");
    // The bridge gate: `b` is both a replacement and a renamed original, so
    // `mcp.docs.b` would denote two different server tools depending on which
    // direction the alias map is read.
    assert!(matches!(
        McpToolSet::new(
            "docs",
            BTreeMap::from([
                ("a".to_owned(), "b".to_owned()),
                ("b".to_owned(), "c".to_owned()),
            ]),
            Arc::clone(&stub) as Arc<dyn McpConnection>,
        ),
        Err(BridgeError::Collision)
    ));
    // The configuration gate, i.e. the same rule where the manager reads it.
    let connector = Arc::new(Stubs::default());
    assert!(matches!(
        McpClientManager::with_ports(
            vec![config_with("docs", &[("a", "b"), ("b", "c")])],
            Arc::new(Secrets::default()),
            connector,
            Arc::new(Clock::default()),
        ),
        Err(McpClientError::Configuration)
    ));
}

#[tokio::test]
async fn a_rejected_replacement_changes_nothing_and_keeps_every_server_dispatchable() {
    let docs = Stub::new("docs");
    let (manager, connector) = manager_with(vec![config_with("docs", &[])], vec![docs.clone()]);
    let handle = manager.contribute(token()).await.unwrap().tools.remove(0);
    assert_eq!(
        text(&handle.execute(context(), json!({})).await.unwrap()),
        "docs"
    );
    let attempts = connector.attempts.load(Ordering::SeqCst);

    // An invalid batch — one good server plus one the validator must refuse.
    let mut broken = config_with("broken", &[]);
    broken.resilience.request_timeout_ms = 0;
    assert_eq!(
        manager
            .replace_configs(vec![config_with("docs", &[]), broken])
            .await,
        Err(McpClientError::Configuration)
    );
    // No state moved: same alias list, no new transport, and the handle minted
    // before the failed call is STILL dispatchable.
    assert_eq!(manager.aliases(), ["docs"]);
    assert_eq!(connector.attempts.load(Ordering::SeqCst), attempts);
    assert_eq!(docs.closes.load(Ordering::SeqCst), 0);
    assert!(
        manager
            .call_tool("mcp.docs.t", model::JsonObject::new(), token())
            .await
            .is_ok()
    );
    assert_eq!(
        text(&handle.execute(context(), json!({})).await.unwrap()),
        "docs"
    );
    assert_eq!(docs.received(), ["t", "t", "t"]);
    manager.dispose().await.unwrap();
}

#[tokio::test]
async fn a_dropped_session_that_refuses_to_close_leaves_the_survivors_usable() {
    let kept = Stub::new("kept");
    let dropped = Stub::failing_close(&Stub::new("dropped"));
    let (manager, _) = manager_with(
        vec![config_with("kept", &[]), config_with("dropped", &[])],
        vec![kept.clone(), dropped.clone()],
    );
    let report = manager.contribute(token()).await.unwrap();
    let mut connected = report.connections.connected.clone();
    connected.sort();
    assert_eq!(connected, ["dropped", "kept"]);
    let kept_handle = report
        .tools
        .iter()
        .find(|tool| tool.definition().capability == "mcp.kept.t")
        .expect("kept tool")
        .clone();

    // Removing the uncooperative server reports the close failure ...
    assert_eq!(
        manager
            .replace_configs(vec![config_with("kept", &[])])
            .await,
        Err(McpClientError::Disconnected)
    );
    // ... but the survivor is untouched: still configured, still dispatchable,
    // and the handle contributed before the swap still reaches its own callback.
    assert_eq!(manager.aliases(), ["kept"]);
    assert!(
        manager
            .call_tool("mcp.kept.t", model::JsonObject::new(), token())
            .await
            .is_ok()
    );
    assert_eq!(
        text(&kept_handle.execute(context(), json!({})).await.unwrap()),
        "kept"
    );
    assert_eq!(kept.received(), ["t", "t"]);
    assert_eq!(
        manager
            .call_tool("mcp.dropped.t", model::JsonObject::new(), token())
            .await,
        Err(McpClientError::Closed)
    );
    assert!(
        dropped.received().is_empty(),
        "the dropped server ran nothing at all, before or after removal"
    );
    manager.dispose().await.unwrap();
}

/// A loopback MCP server that records every `tools/call` it actually received:
/// the only witness that can prove a refused call never reached the wire.
#[derive(Default)]
struct Witness {
    calls: Mutex<Vec<String>>,
    lists: AtomicUsize,
}

#[async_trait]
impl Backend for Witness {
    async fn list_tools(&self, _: &SessionScope) -> Result<Vec<CatalogTool>, rmcp::ErrorData> {
        self.lists.fetch_add(1, Ordering::SeqCst);
        Ok(vec![CatalogTool {
            definition: support::tools().remove(0),
            writes: false,
        }])
    }
    async fn call_tool(
        &self,
        name: &str,
        _: model::JsonObject,
        _: &SessionScope,
    ) -> Result<model::CallToolResult, rmcp::ErrorData> {
        self.calls.lock().unwrap().push(name.into());
        Ok(model::CallToolResult::success(vec![
            model::ContentBlock::text("served"),
        ]))
    }
    fn revoke_actor(&self, _: &str) {}
}

struct Open;
#[async_trait]
impl Authenticator for Open {
    async fn authenticate(
        &self,
        _: &http::HeaderMap,
    ) -> Option<(serde_json::Value, Option<String>)> {
        Some((json!({"id": "owner"}), None))
    }
}

/// Boots the real crate server on loopback and points one manager alias at it.
async fn loopback() -> (Arc<McpClientManager>, Arc<Witness>, McpServer) {
    let witness = Arc::new(Witness::default());
    let server = McpServer::new(witness.clone(), Arc::new(Open), ServerOptions::default())
        .expect("server options are valid");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let router = server.router();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let config: ServerConfig = serde_json::from_value(json!({
        "alias": "docs",
        "transport": {"kind": "http", "url": url,
                      "headers": {"authorization": "Bearer 0123456789abcdef0123456789abcdef"}},
    }))
    .unwrap();
    let manager = McpClientManager::new(vec![config], Arc::new(Secrets::default())).unwrap();
    let manager = Arc::new(manager);
    manager
        .connect("docs", token())
        .await
        .expect("the loopback server accepts a client");
    (manager, witness, server)
}

/// The invariant this guards: the guard/cancellation check is the LAST thing
/// that runs before the transport write. A settlement that lands after the
/// authorization await returned — and before a byte was sent — must stop the
/// call, not merely annotate it.
#[tokio::test]
async fn cancellation_after_the_authorization_check_never_reaches_the_wire() {
    let (manager, witness, server) = loopback().await;
    let handle = manager.contribute(token()).await.unwrap().tools.remove(0);
    assert!(
        witness.calls.lock().unwrap().is_empty(),
        "listing must not dispatch a tool call"
    );

    let cancellation = CancellationToken::new();
    let guard = Arc::new(CancelOnCheck {
        cancellation: cancellation.clone(),
    });
    let result = handle
        .execute(context_with(cancellation, guard), json!({}))
        .await
        .unwrap();
    assert!(!result.ok);
    assert_eq!(error_code(&result), "mcp_request_aborted");
    assert!(
        witness.calls.lock().unwrap().is_empty(),
        "a call cancelled after authorization still reached the server"
    );
    manager.dispose().await.unwrap();
    server.close().await;
}

/// The authority that gates the wire is the one taken where the call is
/// dispatched. A denial there is never bypassed, and one dispatch attempt
/// consults the host's guard exactly once: the earlier shape checked the guard
/// twice per attempt (session-level and again at the connection), which would
/// charge a human approval gate twice for a single tool call.
#[tokio::test]
async fn the_authority_check_at_the_wire_is_what_gates_the_wire() {
    let (manager, witness, server) = loopback().await;
    let handle = manager.contribute(token()).await.unwrap().tools.remove(0);
    let refused = handle
        .execute(context_with(token(), Arc::new(DenyGuard)), json!({}))
        .await
        .unwrap();
    assert!(!refused.ok);
    assert_eq!(error_code(&refused), "mcp_authority_revoked");
    assert!(
        witness.calls.lock().unwrap().is_empty(),
        "a refused authority still reached the server"
    );

    let counting = Arc::new(CountingGuard::default());
    let allowed = handle
        .execute(context_with(token(), counting.clone()), json!({}))
        .await
        .unwrap();
    assert!(allowed.ok, "the allowing guard's call must go through");
    assert_eq!(witness.calls.lock().unwrap().len(), 1);
    assert_eq!(
        counting.checks.load(Ordering::SeqCst),
        1,
        "one dispatch attempt, one host authority consultation"
    );
    manager.dispose().await.unwrap();
    server.close().await;
}

/// And the positive control: with authority intact the very same path serves.
#[tokio::test]
async fn an_authorized_call_through_the_same_path_reaches_the_server() {
    let (manager, witness, server) = loopback().await;
    let handle = manager.contribute(token()).await.unwrap().tools.remove(0);
    let result = handle.execute(context(), json!({})).await.unwrap();
    assert!(result.ok, "{:?}", result.model_data);
    assert_eq!(text(&result), "served");
    assert_eq!(*witness.calls.lock().unwrap(), ["remote"]);
    manager.dispose().await.unwrap();
    server.close().await;
}
