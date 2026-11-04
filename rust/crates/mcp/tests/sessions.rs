//! Session-lifecycle and method ports from
//! `packages/mcp-server/tests/handler.test.ts` ("sessions and scope",
//! "session limits", "in-flight calls", "session binding", and the
//! unsupported-transport-method refusal). The clock here is real time, so
//! every wait is a bounded poll on observable state (`revoke_actor` calls,
//! backend entries), never a fixed sleep past an expected event.

use agentkit_mcp::{
    Authenticator, Backend, BearerAuth, CatalogTool, McpServer, ServerOptions, SessionScope,
    model::*,
};
use async_trait::async_trait;
use rmcp::ErrorData;
use serde_json::{Value, json};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct Host {
    actors: Mutex<Vec<String>>,
    revoked: Mutex<Vec<String>>,
    entered: AtomicUsize,
    gate: CancellationToken,
}
impl Host {
    fn new() -> Self {
        Self {
            gate: CancellationToken::new(),
            ..Default::default()
        }
    }
    fn revoked(&self) -> Vec<String> {
        self.revoked.lock().unwrap().clone()
    }
}
#[async_trait]
impl Backend for Host {
    async fn list_tools(&self, scope: &SessionScope) -> Result<Vec<CatalogTool>, ErrorData> {
        self.actors.lock().unwrap().push(scope.actor_id.clone());
        Ok(vec![CatalogTool {
            definition: serde_json::from_value(json!({
                "name": "block",
                "description": "Blocks until released.",
                "inputSchema": {"type": "object", "properties": {}}
            }))
            .unwrap(),
            writes: false,
        }])
    }
    async fn call_tool(
        &self,
        _name: &str,
        _arguments: JsonObject,
        scope: &SessionScope,
    ) -> Result<CallToolResult, ErrorData> {
        self.entered.fetch_add(1, Ordering::SeqCst);
        tokio::select! {
            _ = self.gate.cancelled() => {}
            _ = scope.cancellation.cancelled() => {}
        }
        Ok(CallToolResult::success(vec![ContentBlock::text(
            "released",
        )]))
    }
    fn revoke_actor(&self, actor: &str) {
        self.revoked.lock().unwrap().push(actor.into());
    }
}

/// Two interchangeable tokens: both authenticate, as different principals.
struct TwoPrincipals;
#[async_trait]
impl Authenticator for TwoPrincipals {
    async fn authenticate(&self, headers: &http::HeaderMap) -> Option<(Value, Option<String>)> {
        match headers.get("authorization")?.to_str().ok()? {
            "Bearer owner-a" => Some((json!({"id":"a"}), None)),
            "Bearer owner-b" => Some((json!({"id":"b"}), None)),
            _ => None,
        }
    }
}

async fn serve(
    options: ServerOptions,
) -> (String, McpServer, Arc<Host>, tokio::task::JoinHandle<()>) {
    serve_with(
        Arc::new(BearerAuth::new("secret", json!({"id":"owner"})).unwrap()),
        options,
    )
    .await
}

async fn serve_with(
    auth: Arc<dyn Authenticator>,
    options: ServerOptions,
) -> (String, McpServer, Arc<Host>, tokio::task::JoinHandle<()>) {
    let host = Arc::new(Host::new());
    let server = McpServer::new(host.clone(), auth, options).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/", listener.local_addr().unwrap());
    let router = server.router();
    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (endpoint, server, host, handle)
}

async fn request(
    client: &reqwest::Client,
    endpoint: &str,
    token: &str,
    method: reqwest::Method,
    session: Option<&str>,
    payload: Option<Value>,
) -> (reqwest::StatusCode, Vec<(String, String)>, String) {
    let mut request = client
        .request(method, endpoint)
        .bearer_auth(token)
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .header("mcp-protocol-version", "2025-11-25");
    if let Some(session) = session {
        request = request.header("mcp-session-id", session);
    }
    if let Some(payload) = payload {
        request = request.json(&payload);
    }
    let response = request.send().await.unwrap();
    let status = response.status();
    let headers = response
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_owned(),
                value.to_str().unwrap_or_default().to_owned(),
            )
        })
        .collect();
    let text = response.text().await.unwrap();
    (status, headers, text)
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

async fn init(client: &reqwest::Client, endpoint: &str, token: &str) -> String {
    let (status, headers, text) = request(
        client,
        endpoint,
        token,
        reqwest::Method::POST,
        None,
        Some(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"sessions","version":"1"}}})),
    )
    .await;
    assert_eq!(status, 200, "{text}");
    headers
        .iter()
        .find(|(name, _)| name == "mcp-session-id")
        .expect("initialize mints a session")
        .1
        .clone()
}

async fn list(
    client: &reqwest::Client,
    endpoint: &str,
    token: &str,
    session: &str,
) -> (reqwest::StatusCode, Value) {
    let (status, _, text) = request(
        client,
        endpoint,
        token,
        reqwest::Method::POST,
        Some(session),
        Some(json!({"jsonrpc":"2.0","id":2,"method":"tools/list"})),
    )
    .await;
    (status, frame(&text))
}

/// Fire a gated `tools/call` without waiting for its answer.
fn gated_call(
    client: &reqwest::Client,
    endpoint: &str,
    session: &str,
) -> tokio::task::JoinHandle<(reqwest::StatusCode, Value)> {
    let client = client.clone();
    let endpoint = endpoint.to_owned();
    let session = session.to_owned();
    tokio::spawn(async move {
        let (status, _, text) = request(
            &client,
            &endpoint,
            "secret",
            reqwest::Method::POST,
            Some(&session),
            Some(json!({"jsonrpc":"2.0","id":7,"method":"tools/call",
                        "params":{"name":"block","arguments":{}}})),
        )
        .await;
        (status, frame(&text))
    })
}

async fn await_entered(host: &Arc<Host>, count: usize) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if host.entered.load(Ordering::SeqCst) >= count {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{count} calls enter the backend"));
}

#[tokio::test]
async fn idle_sessions_are_reaped_lazily_and_free_their_slots() {
    let options = ServerOptions {
        session_idle_ttl: Duration::from_millis(80),
        max_sessions_per_principal: 4,
        ..ServerOptions::default()
    };
    let (endpoint, server, host, handle) = serve(options).await;
    let client = reqwest::Client::new();
    let first = init(&client, &endpoint, "secret").await;
    let second = init(&client, &endpoint, "secret").await;
    // Warm both, then drain the answers: a request still in flight is
    // deliberately not reapable.
    assert_eq!(list(&client, &endpoint, "secret", &first).await.0, 200);
    assert_eq!(list(&client, &endpoint, "secret", &second).await.0, 200);
    let first_actor = host.actors.lock().unwrap()[0].clone();
    let second_actor = host.actors.lock().unwrap()[1].clone();

    // Lazily: it is the NEXT request that sweeps them, so keep sweeping with
    // requests of their own until both are gone.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if host.revoked().len() >= 2 {
                return;
            }
            let sweeper = init(&client, &endpoint, "secret").await;
            let _ = list(&client, &endpoint, "secret", &sweeper).await;
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("idle sessions are reaped by the next request");
    assert_eq!(
        host.revoked()[..2],
        [first_actor, second_actor],
        "the reaper revokes exactly the idle pair"
    );
    assert_eq!(list(&client, &endpoint, "secret", &first).await.0, 404);
    assert_eq!(
        list(&client, &endpoint, "secret", &second).await.0,
        404,
        "a reaped session is gone for good"
    );
    // And they no longer occupy a slot: two fresh sessions fit and neither
    // evicts the other.
    let third = init(&client, &endpoint, "secret").await;
    let fourth = init(&client, &endpoint, "secret").await;
    assert_eq!(list(&client, &endpoint, "secret", &third).await.0, 200);
    assert_eq!(list(&client, &endpoint, "secret", &fourth).await.0, 200);
    server.close().await;
    handle.abort();
}

#[tokio::test]
async fn a_session_with_a_running_call_is_not_reaped() {
    let options = ServerOptions {
        session_idle_ttl: Duration::from_millis(80),
        max_call_duration: Duration::from_secs(5),
        max_sessions_per_principal: 8,
        ..ServerOptions::default()
    };
    let (endpoint, server, host, handle) = serve(options).await;
    let client = reqwest::Client::new();
    let session = init(&client, &endpoint, "secret").await;
    let (status, _) = list(&client, &endpoint, "secret", &session).await;
    assert_eq!(status, 200);
    let actor = host.actors.lock().unwrap().last().unwrap().clone();
    let call = gated_call(&client, &endpoint, &session);
    await_entered(&host, 1).await;

    // Sweep well past the idle TTL: the session has a request in flight, so
    // the answer to that call has to go somewhere.
    let started = Instant::now();
    while started.elapsed() < Duration::from_millis(400) {
        let sweeper = init(&client, &endpoint, "secret").await;
        let _ = list(&client, &endpoint, "secret", &sweeper).await;
        assert!(
            !host.revoked().contains(&actor),
            "a busy session must never be reaped"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(list(&client, &endpoint, "secret", &session).await.0, 200);

    host.gate.cancel();
    let (status, body) = tokio::time::timeout(Duration::from_secs(5), call)
        .await
        .expect("the running call is answered")
        .unwrap();
    assert_eq!(status, 200, "{body}");
    assert!(body.get("result").is_some(), "{body}");
    server.close().await;
    handle.abort();
}

#[tokio::test]
async fn a_pinned_call_is_reaped_only_past_its_deadline_plus_the_idle_ttl() {
    let options = ServerOptions {
        session_idle_ttl: Duration::from_millis(320),
        max_call_duration: Duration::from_millis(600),
        max_sessions_per_principal: 8,
        ..ServerOptions::default()
    };
    let (endpoint, server, host, handle) = serve(options).await;
    let client = reqwest::Client::new();
    let session = init(&client, &endpoint, "secret").await;
    let (status, _) = list(&client, &endpoint, "secret", &session).await;
    assert_eq!(status, 200);
    let actor = host.actors.lock().unwrap().last().unwrap().clone();
    let call = gated_call(&client, &endpoint, &session);
    await_entered(&host, 1).await;

    // Still inside maxCallDuration + idle TTL: a slow call is not a stuck one.
    let started = Instant::now();
    while started.elapsed() < Duration::from_millis(450) {
        let sweeper = init(&client, &endpoint, "secret").await;
        let _ = list(&client, &endpoint, "secret", &sweeper).await;
        assert!(
            !host.revoked().contains(&actor),
            "a call inside its deadline must not be reaped early"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // Past both: the deadline releases the handler and the next sweep reaps.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if host.revoked().contains(&actor) {
                return;
            }
            let sweeper = init(&client, &endpoint, "secret").await;
            let _ = list(&client, &endpoint, "secret", &sweeper).await;
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("a call that outlived its deadline plus a whole TTL is reaped");
    let _ = call.await;
    server.close().await;
    handle.abort();
}

#[tokio::test]
async fn a_new_session_is_refused_with_503_and_retry_after_when_all_are_busy() {
    let options = ServerOptions {
        max_sessions_per_principal: 1,
        max_call_duration: Duration::from_secs(5),
        ..ServerOptions::default()
    };
    let (endpoint, server, host, handle) = serve(options).await;
    let client = reqwest::Client::new();
    let session = init(&client, &endpoint, "secret").await;
    let call = gated_call(&client, &endpoint, &session);
    await_entered(&host, 1).await;

    // The only session under the cap is in a tool call, so nothing is
    // evictable — going over the cap is what let one caller hold unbounded
    // busy sessions, so the newcomer is refused instead.
    let (status, headers, text) = request(
        &client,
        &endpoint,
        "secret",
        reqwest::Method::POST,
        None,
        Some(json!({"jsonrpc":"2.0","id":9,"method":"initialize","params":{
            "protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"late","version":"1"}}})),
    )
    .await;
    assert_eq!(status, 503, "{text}");
    assert!(
        headers
            .iter()
            .any(|(name, value)| name == "retry-after" && value == "5"),
        "admission control is temporary; the refusal must say when to return: {headers:?}"
    );
    let body = frame(&text);
    assert_eq!(body["error"]["code"], -32000, "{body}");

    // And the live session is untouched: its answer still arrives.
    host.gate.cancel();
    let (status, body) = tokio::time::timeout(Duration::from_secs(5), call)
        .await
        .expect("the running call is answered")
        .unwrap();
    assert_eq!(status, 200, "{body}");
    assert!(body.get("result").is_some(), "{body}");
    assert_eq!(list(&client, &endpoint, "secret", &session).await.0, 200);
    server.close().await;
    handle.abort();
}

#[tokio::test]
async fn a_deleted_session_is_forgotten_forever() {
    let (endpoint, server, host, handle) = serve(ServerOptions::default()).await;
    let client = reqwest::Client::new();
    let session = init(&client, &endpoint, "secret").await;
    let (status, _) = list(&client, &endpoint, "secret", &session).await;
    assert_eq!(status, 200);
    let actor = host.actors.lock().unwrap().last().unwrap().clone();
    let (status, _, _) = request(
        &client,
        &endpoint,
        "secret",
        reqwest::Method::DELETE,
        Some(&session),
        None,
    )
    .await;
    assert!(status.is_success(), "DELETE must succeed: {status}");
    assert!(
        host.revoked().contains(&actor),
        "the explicit teardown revokes the actor"
    );
    for _ in 0..2 {
        assert_eq!(
            list(&client, &endpoint, "secret", &session).await.0,
            404,
            "a deleted session stays deleted"
        );
    }
    server.close().await;
    handle.abort();
}

#[tokio::test]
async fn a_foreign_delete_is_refused_and_leaves_the_owner_live() {
    let (endpoint, server, _host, handle) =
        serve_with(Arc::new(TwoPrincipals), ServerOptions::default()).await;
    let client = reqwest::Client::new();
    let session = init(&client, &endpoint, "owner-a").await;
    let (status, _, _) = request(
        &client,
        &endpoint,
        "owner-b",
        reqwest::Method::DELETE,
        Some(&session),
        None,
    )
    .await;
    assert_eq!(
        status, 404,
        "a leaked id is indistinguishable from an unknown one"
    );
    // The owner still has it: the refusal cost B nothing and A nothing.
    assert_eq!(list(&client, &endpoint, "owner-a", &session).await.0, 200);
    server.close().await;
    handle.abort();
}

#[tokio::test]
async fn eviction_closes_the_victim_s_live_sse_stream() {
    let options = ServerOptions {
        max_sessions_per_principal: 2,
        ..ServerOptions::default()
    };
    let (endpoint, server, _host, handle) = serve(options).await;
    let client = reqwest::Client::new();
    let victim = init(&client, &endpoint, "secret").await;
    // A standalone SSE stream on the victim: the eviction has to CLOSE the
    // transport, not just forget the map entry — an ended stream is the only
    // observable difference between the two.
    let response = tokio::time::timeout(
        Duration::from_secs(5),
        client
            .get(&endpoint)
            .bearer_auth("secret")
            .header("accept", "text/event-stream")
            .header("mcp-session-id", &victim)
            .header("mcp-protocol-version", "2025-11-25")
            .send(),
    )
    .await
    .expect("bounded GET")
    .unwrap();
    assert_eq!(response.status(), 200);

    // Push the victim out from under the cap with two more sessions.
    let second = init(&client, &endpoint, "secret").await;
    let third = init(&client, &endpoint, "secret").await;
    assert_eq!(list(&client, &endpoint, "secret", &second).await.0, 200);
    assert_eq!(list(&client, &endpoint, "secret", &third).await.0, 200);

    // The victim's stream ended: reads drain to None, within a bound.
    let mut response = response;
    let ended = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match tokio::time::timeout(Duration::from_millis(100), response.chunk()).await {
                Ok(Ok(None)) | Ok(Err(_)) => return true,
                Ok(Ok(Some(_))) | Err(_) => {}
            }
        }
    })
    .await
    .unwrap_or(false);
    assert!(ended, "eviction must close the live SSE stream");
    assert_eq!(list(&client, &endpoint, "secret", &victim).await.0, 404);
    server.close().await;
    handle.abort();
}

#[tokio::test]
async fn each_actor_is_revoked_exactly_once_across_eviction_and_shutdown() {
    let options = ServerOptions {
        max_sessions_per_principal: 1,
        ..ServerOptions::default()
    };
    let (endpoint, server, host, handle) = serve(options).await;
    let client = reqwest::Client::new();
    let first = init(&client, &endpoint, "secret").await;
    let (status, _) = list(&client, &endpoint, "secret", &first).await;
    assert_eq!(status, 200);
    let first_actor = host.actors.lock().unwrap()[0].clone();
    let second = init(&client, &endpoint, "secret").await;
    let (status, _) = list(&client, &endpoint, "secret", &second).await;
    assert_eq!(status, 200);
    let second_actor = host.actors.lock().unwrap()[1].clone();
    assert_ne!(first_actor, second_actor);

    // The eviction released exactly one actor: the victim, once.
    assert_eq!(
        host.revoked(),
        [first_actor.as_str()],
        "the evicted actor is revoked exactly once"
    );
    // Awaited shutdown releases the survivor; a second dispose is idempotent.
    server.close().await;
    server.close().await;
    assert_eq!(
        host.revoked(),
        [first_actor, second_actor],
        "each actor is released exactly once across eviction and shutdown"
    );
    handle.abort();
}

#[tokio::test]
async fn unsupported_methods_are_refused_only_after_authenticating() {
    let (endpoint, server, _host, handle) = serve(ServerOptions::default()).await;
    let client = reqwest::Client::new();
    for method in [
        reqwest::Method::PUT,
        reqwest::Method::PATCH,
        reqwest::Method::OPTIONS,
    ] {
        let anonymous = tokio::time::timeout(
            Duration::from_secs(5),
            client
                .request(method.clone(), &endpoint)
                .header("accept", "application/json, text/event-stream")
                .send(),
        )
        .await
        .expect("bounded request")
        .unwrap();
        assert_eq!(anonymous.status(), 401, "{method} must authenticate first");
        assert_eq!(
            anonymous
                .headers()
                .get("www-authenticate")
                .expect("the 401 carries the challenge")
                .to_str()
                .unwrap(),
            "Bearer realm=\"agentkit-mcp\""
        );
        assert_eq!(
            anonymous.text().await.unwrap(),
            "",
            "the anonymous refusal is bodyless"
        );
        let (status, headers, text) =
            request(&client, &endpoint, "secret", method.clone(), None, None).await;
        assert_eq!(status, 405, "{method} is not an MCP transport method");
        assert!(
            headers
                .iter()
                .any(|(name, value)| name == "allow" && value == "GET, POST, DELETE"),
            "the refusal must advertise the supported methods: {headers:?}"
        );
        assert_eq!(frame(&text)["error"]["code"], -32000, "{text}");
    }
    server.close().await;
    handle.abort();
}

#[tokio::test]
async fn an_unsupported_session_protocol_version_header_is_refused_with_400() {
    let (endpoint, server, _host, handle) = serve(ServerOptions::default()).await;
    let client = reqwest::Client::new();
    let session = init(&client, &endpoint, "secret").await;
    let response = tokio::time::timeout(
        Duration::from_secs(5),
        client
            .post(&endpoint)
            .bearer_auth("secret")
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json")
            .header("mcp-session-id", &session)
            .header("mcp-protocol-version", "3000-01-01")
            .json(&json!({"jsonrpc":"2.0","id":3,"method":"tools/list"}))
            .send(),
    )
    .await
    .expect("bounded request")
    .unwrap();
    assert_eq!(response.status(), 400);
    let text = response.text().await.unwrap();
    assert_eq!(
        frame(&text)["error"]["code"],
        -32600,
        "the refusal is an Invalid Request: {text}"
    );
    // The session itself is untouched by the refused request.
    assert_eq!(list(&client, &endpoint, "secret", &session).await.0, 200);
    server.close().await;
    handle.abort();
}

#[tokio::test]
async fn concurrent_initializes_never_exceed_the_session_cap() {
    const CAP: usize = 3;
    const EXTRA: usize = 3;
    let options = ServerOptions {
        max_sessions_per_principal: CAP,
        max_call_duration: Duration::from_secs(5),
        ..ServerOptions::default()
    };
    let (endpoint, server, _host, handle) = serve(options).await;
    let client = reqwest::Client::new();
    let initialize = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
        "protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"race","version":"1"}}})
    .to_string();
    let mut attempts = Vec::new();
    for _ in 0..CAP + EXTRA {
        let (client, endpoint, initialize) = (client.clone(), endpoint.clone(), initialize.clone());
        attempts.push(tokio::spawn(async move {
            // The body arrives late, so every admitted initialize stays in
            // flight (holding its reserved slot) while the others are judged.
            let body = reqwest::Body::wrap_stream(futures::stream::once(async move {
                tokio::time::sleep(Duration::from_millis(600)).await;
                Ok::<_, std::io::Error>(initialize)
            }));
            client
                .post(&endpoint)
                .bearer_auth("secret")
                .header("accept", "application/json, text/event-stream")
                .header("content-type", "application/json")
                .header("mcp-protocol-version", "2025-11-25")
                .body(body)
                .send()
                .await
                .unwrap()
                .status()
        }));
    }
    let mut statuses = Vec::new();
    for attempt in attempts {
        statuses.push(attempt.await.unwrap().as_u16());
    }
    assert_eq!(
        statuses.iter().filter(|s| **s == 200).count(),
        CAP,
        "{statuses:?}"
    );
    assert_eq!(
        statuses.iter().filter(|s| **s == 503).count(),
        EXTRA,
        "{statuses:?}"
    );
    server.close().await;
    handle.abort();
}
