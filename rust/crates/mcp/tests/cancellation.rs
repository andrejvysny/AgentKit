//! Request-cancellation port from `packages/mcp-server/tests/
//! request-cancellation.test.ts`: cancelling a live request reaches the
//! running tool, a queued call is never dispatched, and the session stays
//! usable. The TypeScript suite runs the same body for `legacy` and `auto`;
//! the native lane only ever speaks the sessionful (legacy) transport — the
//! `auto`/stateless lane is a documented drop, so this is its sibling. Both
//! cancellation surfaces of that transport get their own test: the
//! MCP-native `notifications/cancelled` an SDK client sends when its request
//! signal aborts, and the bare transport hang-up — abandoning the SSE
//! response stream mid-call with no protocol-level message at all.

use agentkit_mcp::{
    Backend, BearerAuth, CatalogTool, McpServer, ServerOptions, SessionScope, model::*,
};
use async_trait::async_trait;
use rmcp::ErrorData;
use serde_json::{Value, json};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct Host {
    executions: Mutex<Vec<String>>,
    /// Scope tokens handed to each entered call; the server must cancel the
    /// running call's scope once its request connection is aborted.
    scopes: Mutex<Vec<(String, CancellationToken)>>,
    entered: AtomicUsize,
    /// Handler-level admissions (pre-permit), wired by `serve`: a queued
    /// call counts here but never reaches `entered`, which is exactly the
    /// distinction the queued-leg assertions must observe.
    admissions: Arc<AtomicUsize>,
    gate: CancellationToken,
}
impl Host {
    fn new() -> Self {
        Self {
            gate: CancellationToken::new(),
            ..Default::default()
        }
    }
}
#[async_trait]
impl Backend for Host {
    async fn list_tools(&self, _scope: &SessionScope) -> Result<Vec<CatalogTool>, ErrorData> {
        Ok(vec![CatalogTool {
            definition: serde_json::from_value(json!({
                "name": "block",
                "description": "Blocks until released.",
                "inputSchema": {"type": "object", "properties": {
                    "label": {"type": "string"}}}
            }))
            .unwrap(),
            writes: false,
        }])
    }
    async fn call_tool(
        &self,
        _name: &str,
        arguments: JsonObject,
        scope: &SessionScope,
    ) -> Result<CallToolResult, ErrorData> {
        let label = arguments["label"].as_str().unwrap_or("?").to_owned();
        self.executions.lock().unwrap().push(label.clone());
        self.scopes
            .lock()
            .unwrap()
            .push((label.clone(), scope.cancellation.clone()));
        self.entered.fetch_add(1, Ordering::SeqCst);
        tokio::select! {
            _ = self.gate.cancelled() => {}
            _ = scope.cancellation.cancelled() => {}
        }
        Ok(CallToolResult::success(vec![ContentBlock::text(
            "released",
        )]))
    }
    fn revoke_actor(&self, _actor: &str) {}
}

async fn serve(
    mut options: ServerOptions,
) -> (String, McpServer, Arc<Host>, tokio::task::JoinHandle<()>) {
    let host = Arc::new(Host::new());
    let counter = Arc::clone(&host.admissions);
    options.call_admissions = Some(Arc::new(move || {
        counter.fetch_add(1, Ordering::SeqCst);
    }));
    let server = McpServer::new(
        host.clone(),
        Arc::new(BearerAuth::new("secret", json!({"id":"owner"})).unwrap()),
        options,
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/", listener.local_addr().unwrap());
    let router = server.router();
    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (endpoint, server, host, handle)
}

async fn send(
    client: &reqwest::Client,
    endpoint: &str,
    session: Option<&str>,
    payload: Value,
) -> reqwest::Response {
    tokio::time::timeout(
        Duration::from_secs(10),
        client
            .post(endpoint)
            .bearer_auth("secret")
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json")
            .header("mcp-protocol-version", "2025-11-25")
            .headers_session(session)
            .json(&payload)
            .send(),
    )
    .await
    .expect("bounded request")
    .unwrap()
}

trait HeadersSession {
    fn headers_session(self, session: Option<&str>) -> Self;
}
impl HeadersSession for reqwest::RequestBuilder {
    fn headers_session(self, session: Option<&str>) -> Self {
        match session {
            Some(session) => self.header("mcp-session-id", session),
            None => self,
        }
    }
}

fn frame(text: &str) -> Value {
    serde_json::from_str(text)
        .ok()
        .or_else(|| {
            text.lines()
                .filter_map(|line| {
                    line.strip_prefix("data:")
                        .and_then(|data| serde_json::from_str(data.trim()).ok())
                })
                .next_back()
        })
        .unwrap_or(Value::Null)
}

async fn post(
    client: &reqwest::Client,
    endpoint: &str,
    session: Option<&str>,
    payload: Value,
) -> Value {
    let response = send(client, endpoint, session, payload).await;
    let text = tokio::time::timeout(Duration::from_secs(10), response.text())
        .await
        .expect("bounded body")
        .unwrap();
    frame(&text)
}

#[tokio::test]
async fn cancelling_a_running_call_reaches_the_tool_and_prevents_the_queued_call() {
    let options = ServerOptions {
        max_concurrent_calls: 1,
        max_call_duration: Duration::from_secs(30),
        ..ServerOptions::default()
    };
    let (endpoint, server, host, handle) = serve(options).await;
    let client = reqwest::Client::new();
    let response = send(
        &client,
        &endpoint,
        None,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"cancel","version":"1"}}}),
    )
    .await;
    let session = response
        .headers()
        .get("mcp-session-id")
        .expect("initialize mints a session")
        .to_str()
        .unwrap()
        .to_owned();
    drop(response.text().await);
    let _ = post(
        &client,
        &endpoint,
        Some(&session),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
    )
    .await;

    // The running call: gated inside the backend until cancelled. Its own
    // client, so dropping the queued connection cannot abort it too.
    let running = {
        let client = reqwest::Client::new();
        let endpoint = endpoint.clone();
        let session = session.clone();
        tokio::spawn(async move {
            let _ = post(
                &client,
                &endpoint,
                Some(&session),
                json!({"jsonrpc":"2.0","id":11,"method":"tools/call",
                       "params":{"name":"block","arguments":{"label":"first"}}}),
            )
            .await;
        })
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if host.entered.load(Ordering::SeqCst) >= 1 {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the first call enters the backend");

    // The queued call: parked behind the single permit, on its own
    // connection. The baseline lets the test prove the message actually
    // reached the handler (queued behind the permit) — cancelling a request
    // the server never admitted would pass the queued leg vacuously.
    let queued_at = host.admissions.load(Ordering::SeqCst);
    let queued = {
        let client = reqwest::Client::new();
        let endpoint = endpoint.clone();
        let session = session.clone();
        tokio::spawn(async move {
            let _ = post(
                &client,
                &endpoint,
                Some(&session),
                json!({"jsonrpc":"2.0","id":12,"method":"tools/call",
                       "params":{"name":"block","arguments":{"label":"queued"}}}),
            )
            .await;
        })
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if host.admissions.load(Ordering::SeqCst) >= queued_at + 1 {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the queued call is admitted by the handler");
    assert_eq!(
        host.entered.load(Ordering::SeqCst),
        1,
        "the queued call must wait behind the single permit, not enter the backend"
    );
    let _ = post(
        &client,
        &endpoint,
        Some(&session),
        json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":12}}),
    )
    .await;
    queued.abort(); // …and drop the connection, like an aborted fetch.

    let _ = post(
        &client,
        &endpoint,
        Some(&session),
        json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":11}}),
    )
    .await;
    running.abort(); // …and hang up on it too, like the aborted fetch.
    let _ = running.await;

    // The running call's scope carries the cancellation to the tool…
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if host
                .scopes
                .lock()
                .unwrap()
                .first()
                .is_some_and(|(_, token)| token.is_cancelled())
            {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the cancellation must reach the running tool's scope");
    {
        let scopes = host.scopes.lock().unwrap();
        assert_eq!(
            scopes
                .iter()
                .map(|(label, _)| label.as_str())
                .collect::<Vec<_>>(),
            ["first"],
            "only the running call may reach the backend"
        );
        assert!(
            scopes[0].1.is_cancelled(),
            "the running tool's scope must be cancelled"
        );
    }
    // …and the queued call never ran at all.
    assert_eq!(
        *host.executions.lock().unwrap(),
        ["first".to_owned()],
        "a cancelled queued call must never be dispatched"
    );

    // The session is still usable: release the gate and finish a call.
    host.gate.cancel();
    let tools = post(
        &client,
        &endpoint,
        Some(&session),
        json!({"jsonrpc":"2.0","id":13,"method":"tools/list"}),
    )
    .await;
    assert_eq!(tools["result"]["tools"][0]["name"], "block", "{tools}");
    let done = post(
        &client,
        &endpoint,
        Some(&session),
        json!({"jsonrpc":"2.0","id":14,"method":"tools/call",
               "params":{"name":"block","arguments":{"label":"after"}}}),
    )
    .await;
    assert_ne!(done["result"]["isError"], json!(true), "{done}");
    assert_eq!(*host.executions.lock().unwrap(), ["first", "after"]);
    server.close().await;
    handle.abort();
}

#[tokio::test]
async fn hanging_up_on_the_sse_response_alone_reaches_the_tool_and_stops_the_queued_call() {
    // The transport-level sibling of the notification-driven case above: no
    // `notifications/cancelled` is ever sent. The only signal the server can
    // observe is the abandoned SSE response stream it is still holding open
    // while the tool blocks — the one signal that must survive a client
    // hanging up mid `tools/call`.
    let options = ServerOptions {
        max_concurrent_calls: 1,
        max_call_duration: Duration::from_secs(30),
        ..ServerOptions::default()
    };
    let (endpoint, server, host, handle) = serve(options).await;
    let client = reqwest::Client::new();
    let response = send(
        &client,
        &endpoint,
        None,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"hangup","version":"1"}}}),
    )
    .await;
    let session = response
        .headers()
        .get("mcp-session-id")
        .expect("initialize mints a session")
        .to_str()
        .unwrap()
        .to_owned();
    drop(response.text().await);
    let _ = post(
        &client,
        &endpoint,
        Some(&session),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
    )
    .await;

    // The running call: gated inside the backend, its SSE response open.
    let running = {
        let client = reqwest::Client::new();
        let endpoint = endpoint.clone();
        let session = session.clone();
        tokio::spawn(async move {
            let _ = post(
                &client,
                &endpoint,
                Some(&session),
                json!({"jsonrpc":"2.0","id":21,"method":"tools/call",
                       "params":{"name":"block","arguments":{"label":"first"}}}),
            )
            .await;
        })
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if host.entered.load(Ordering::SeqCst) >= 1 {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the first call enters the backend");

    // The queued call: parked behind the single permit, on its own stream.
    // The admission baseline proves the hang-up targets a request the
    // server really admitted (a never-admitted request would make the
    // queued leg pass vacuously).
    let queued_at = host.admissions.load(Ordering::SeqCst);
    let queued = {
        let client = reqwest::Client::new();
        let endpoint = endpoint.clone();
        let session = session.clone();
        tokio::spawn(async move {
            let _ = post(
                &client,
                &endpoint,
                Some(&session),
                json!({"jsonrpc":"2.0","id":22,"method":"tools/call",
                       "params":{"name":"block","arguments":{"label":"queued"}}}),
            )
            .await;
        })
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if host.admissions.load(Ordering::SeqCst) >= queued_at + 1 {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the queued call is admitted by the handler");
    assert_eq!(
        host.entered.load(Ordering::SeqCst),
        1,
        "the queued call must wait behind the single permit, not enter the backend"
    );
    // Hang up on both — and only on the transport. No protocol-level
    // cancellation follows.
    queued.abort();
    running.abort();
    let _ = queued.await;
    let _ = running.await;

    // The abandoned streams alone must cancel the running tool's scope…
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if host
                .scopes
                .lock()
                .unwrap()
                .first()
                .is_some_and(|(_, token)| token.is_cancelled())
            {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the hang-up must reach the running tool's scope");
    {
        let scopes = host.scopes.lock().unwrap();
        assert_eq!(
            scopes
                .iter()
                .map(|(label, _)| label.as_str())
                .collect::<Vec<_>>(),
            ["first"],
            "a call cancelled before it was dispatched never enters the backend"
        );
        assert!(
            scopes[0].1.is_cancelled(),
            "the running tool's scope must be cancelled by the hang-up"
        );
    }
    assert_eq!(
        *host.executions.lock().unwrap(),
        ["first".to_owned()],
        "a hung-up queued call must never be dispatched"
    );

    // The session survives its abandoned streams: it still answers and calls.
    host.gate.cancel();
    let tools = post(
        &client,
        &endpoint,
        Some(&session),
        json!({"jsonrpc":"2.0","id":23,"method":"tools/list"}),
    )
    .await;
    assert_eq!(tools["result"]["tools"][0]["name"], "block", "{tools}");
    let done = post(
        &client,
        &endpoint,
        Some(&session),
        json!({"jsonrpc":"2.0","id":24,"method":"tools/call",
               "params":{"name":"block","arguments":{"label":"after"}}}),
    )
    .await;
    assert_ne!(done["result"]["isError"], json!(true), "{done}");
    assert_eq!(*host.executions.lock().unwrap(), ["first", "after"]);
    server.close().await;
    handle.abort();
}
