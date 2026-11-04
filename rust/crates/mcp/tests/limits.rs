//! Request-limit ports from `packages/mcp-server/tests/handler.test.ts`
//! ("request limits"): an oversized batch is refused as one protocol error
//! without dispatching any of it, `maxConcurrentCallsPerSession` caps the
//! peak of concurrent calls on one session, and an oversized CHUNKED body —
//! which declares no length at all — is refused with 413 and mints no session.

use agentkit_mcp::{
    Backend, BearerAuth, CatalogTool, McpServer, ServerOptions, SessionScope, model::*,
};
use async_trait::async_trait;
use axum::body::Bytes;
use futures::stream;
use rmcp::ErrorData;
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct Host {
    /// Every entry into `call_tool`, including gated ones.
    entered: AtomicUsize,
    /// Calls currently inside `call_tool`.
    running: AtomicUsize,
    /// High-water mark of `running`.
    peak: AtomicUsize,
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
                "inputSchema": {"type": "object", "properties": {}}
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
        self.entered.fetch_add(1, Ordering::SeqCst);
        let running = self.running.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(running, Ordering::SeqCst);
        tokio::select! {
            _ = self.gate.cancelled() => {}
            _ = scope.cancellation.cancelled() => {}
        }
        self.running.fetch_sub(1, Ordering::SeqCst);
        let text = if arguments.get("label").and_then(Value::as_str) == Some("big") {
            "b".repeat(4096)
        } else {
            "released".to_owned()
        };
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }
    fn revoke_actor(&self, _actor: &str) {}
}

async fn serve(
    options: ServerOptions,
) -> (String, McpServer, Arc<Host>, tokio::task::JoinHandle<()>) {
    let host = Arc::new(Host::new());
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

async fn rpc(
    client: &reqwest::Client,
    endpoint: &str,
    session: Option<&str>,
    payload: Value,
) -> (reqwest::StatusCode, Option<String>, Value) {
    let mut request = client
        .post(endpoint)
        .bearer_auth("secret")
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .header("mcp-protocol-version", "2025-11-25");
    if let Some(session) = session {
        request = request.header("mcp-session-id", session);
    }
    let response = request.json(&payload).send().await.unwrap();
    let status = response.status();
    let session = response
        .headers()
        .get("mcp-session-id")
        .map(|value| value.to_str().unwrap().to_owned());
    let text = response.text().await.unwrap();
    let body = serde_json::from_str(&text)
        .ok()
        .or_else(|| {
            text.lines()
                .filter_map(|line| {
                    line.strip_prefix("data:")
                        .and_then(|data| serde_json::from_str(data.trim()).ok())
                })
                .next_back()
        })
        .unwrap_or(Value::Null);
    (status, session, body)
}

async fn initialize(client: &reqwest::Client, endpoint: &str) -> String {
    let (status, session, body) = rpc(
        client,
        endpoint,
        None,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"limits","version":"1"}}}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    session.unwrap()
}

#[tokio::test]
async fn oversized_batch_is_refused_as_one_protocol_error_with_zero_dispatch() {
    let options = ServerOptions {
        max_batch_size: 2,
        ..ServerOptions::default()
    };
    let (endpoint, server, host, handle) = serve(options).await;
    let client = reqwest::Client::new();
    let session = initialize(&client, &endpoint).await;
    let batch: Vec<Value> = (0..25)
        .map(|index| {
            json!({"jsonrpc":"2.0","id":index + 10,"method":"tools/call",
                   "params":{"name":"block","arguments":{}}})
        })
        .collect();
    let (status, _, body) = rpc(&client, &endpoint, Some(&session), json!(batch)).await;
    assert_eq!(status, 400);
    assert_eq!(
        body["error"]["code"], -32600,
        "the refusal must be an Invalid Request: {body}"
    );
    // The refusal is the whole point: not one of the 25 reached a tool.
    assert_eq!(
        host.entered.load(Ordering::SeqCst),
        0,
        "a refused batch must not dispatch a single message"
    );
    server.close().await;
    handle.abort();
}

#[tokio::test]
async fn concurrent_calls_on_one_session_are_capped_at_max_concurrent_calls() {
    let options = ServerOptions {
        max_concurrent_calls: 2,
        ..ServerOptions::default()
    };
    let (endpoint, server, host, handle) = serve(options).await;
    let client = reqwest::Client::new();
    let session = initialize(&client, &endpoint).await;
    let calls: Vec<_> = (0..6)
        .map(|index| {
            let client = client.clone();
            let endpoint = endpoint.clone();
            let session = session.clone();
            tokio::spawn(async move {
                rpc(
                    &client,
                    &endpoint,
                    Some(&session),
                    json!({"jsonrpc":"2.0","id":index,"method":"tools/call",
                           "params":{"name":"block","arguments":{}}}),
                )
                .await
            })
        })
        .collect();
    // Two calls enter; a third must not join them while the gate is closed.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if host.entered.load(Ordering::SeqCst) >= 2 {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("two calls enter the backend");
    let third = tokio::time::timeout(Duration::from_millis(200), async {
        loop {
            if host.running.load(Ordering::SeqCst) > 2 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(
        third.is_err(),
        "a third concurrent call may not pass the per-session cap"
    );
    // Released, the queue drains and every message still gets its answer.
    host.gate.cancel();
    for (index, call) in calls.into_iter().enumerate() {
        let (status, _, body) = tokio::time::timeout(Duration::from_secs(5), call)
            .await
            .unwrap_or_else(|_| panic!("call {index} must be answered"))
            .unwrap();
        assert_eq!(status, 200, "call {index}: {body}");
        assert!(
            body.get("result").is_some(),
            "call {index} must get its own answer: {body}"
        );
    }
    assert_eq!(
        host.peak.load(Ordering::SeqCst),
        2,
        "the cap holds for the whole drain, not only its start"
    );
    server.close().await;
    handle.abort();
}

#[tokio::test]
async fn a_batch_runs_at_most_max_concurrent_calls_of_its_messages_at_a_time() {
    // The verbatim frozen shape: ONE batch of six `tools/call` messages on
    // one session, a cap of two — the peak observed inside the backend must
    // be two, and releasing it must still answer every message.
    let options = ServerOptions {
        max_concurrent_calls: 2,
        max_batch_size: 6,
        max_call_duration: Duration::from_secs(30),
        ..ServerOptions::default()
    };
    let (endpoint, server, host, handle) = serve(options).await;
    let client = reqwest::Client::new();
    let session = initialize(&client, &endpoint).await;
    let batch: Vec<Value> = (0..6)
        .map(|index| {
            json!({"jsonrpc":"2.0","id":index + 10,"method":"tools/call",
                   "params":{"name":"block","arguments":{}}})
        })
        .collect();
    let response = tokio::time::timeout(
        Duration::from_secs(5),
        client
            .post(&endpoint)
            .bearer_auth("secret")
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json")
            .header("mcp-protocol-version", "2025-11-25")
            .header("mcp-session-id", &session)
            .json(&batch)
            .send(),
    )
    .await
    .expect("bounded batch request")
    .unwrap();
    // Two messages enter; a third may not join them while the gate is closed.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if host.entered.load(Ordering::SeqCst) >= 2 {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("two batch messages enter the backend");
    let over_cap = tokio::time::timeout(Duration::from_millis(200), async {
        loop {
            if host.running.load(Ordering::SeqCst) > 2 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(
        over_cap.is_err(),
        "a third batch message may not pass the cap"
    );
    // Released, the queue drains and every message still gets its answer.
    host.gate.cancel();
    let text = tokio::time::timeout(Duration::from_secs(10), response.text())
        .await
        .expect("bounded batch body")
        .unwrap();
    let frames: Vec<Value> = text
        .lines()
        .filter_map(|line| {
            line.strip_prefix("data:")
                .and_then(|data| serde_json::from_str(data.trim()).ok())
        })
        .collect();
    let mut ids: Vec<i64> = frames
        .iter()
        .filter(|frame| frame.get("result").is_some())
        .filter_map(|frame| frame["id"].as_i64())
        .collect();
    ids.sort_unstable();
    assert_eq!(
        ids,
        (10..16).collect::<Vec<i64>>(),
        "every message of the batch answers: {frames:?}"
    );
    assert_eq!(
        host.peak.load(Ordering::SeqCst),
        2,
        "the cap holds for the whole drain, not only its start"
    );
    server.close().await;
    handle.abort();
}

#[tokio::test]
async fn oversized_batch_answers_fold_as_generic_protocol_errors() {
    // The fold's per-member answer cap: a member whose answer exceeds
    // `max_request_bytes` is replaced by a generic -32603 frame that keeps
    // the member's id — never silently dropped, so a batch always answers
    // every message exactly once.
    let options = ServerOptions {
        max_request_bytes: 2048,
        max_batch_size: 4,
        max_concurrent_calls: 4,
        max_call_duration: Duration::from_secs(30),
        ..ServerOptions::default()
    };
    let (endpoint, server, host, handle) = serve(options).await;
    host.gate.cancel(); // answer immediately; only the SIZES differ here
    let client = reqwest::Client::new();
    let session = initialize(&client, &endpoint).await;
    let batch = json!([
        {"jsonrpc":"2.0","id":30,"method":"tools/call",
         "params":{"name":"block","arguments":{"label":"small"}}},
        {"jsonrpc":"2.0","id":31,"method":"tools/call",
         "params":{"name":"block","arguments":{"label":"big"}}},
    ]);
    let response = tokio::time::timeout(
        Duration::from_secs(5),
        client
            .post(&endpoint)
            .bearer_auth("secret")
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json")
            .header("mcp-protocol-version", "2025-11-25")
            .header("mcp-session-id", &session)
            .json(&batch)
            .send(),
    )
    .await
    .expect("bounded batch request")
    .unwrap();
    let text = tokio::time::timeout(Duration::from_secs(10), response.text())
        .await
        .expect("bounded batch body")
        .unwrap();
    let frames: Vec<Value> = text
        .lines()
        .filter_map(|line| {
            line.strip_prefix("data:")
                .and_then(|data| serde_json::from_str(data.trim()).ok())
        })
        .collect();
    let frame_for = |id: i64| -> Value {
        frames
            .iter()
            .find(|frame| frame["id"].as_i64() == Some(id))
            .cloned()
            .unwrap_or_else(|| panic!("message {id} must be answered exactly once: {frames:?}"))
    };
    assert!(
        frame_for(30).get("result").is_some(),
        "the small member answers normally: {frames:?}"
    );
    let big = frame_for(31);
    assert_eq!(
        big["error"]["code"].as_i64(),
        Some(-32603),
        "the oversized answer must fold as a generic internal error: {big}"
    );
    assert!(
        big["error"]["message"].is_string(),
        "the replacement names a message, not the payload: {big}"
    );
    assert_eq!(
        frames.len(),
        2,
        "every member is answered exactly once: {frames:?}"
    );
    server.close().await;
    handle.abort();
}

#[tokio::test]
async fn oversized_chunked_body_is_refused_with_413_and_mints_no_session() {
    let options = ServerOptions {
        max_request_bytes: 1024,
        ..ServerOptions::default()
    };
    let (endpoint, server, host, handle) = serve(options).await;
    // A chunked body declares no length: eight 512-byte chunks streamed with
    // an unknown total size, so there is no Content-Length to trust.
    let chunks =
        stream::iter((0..8).map(|_| Ok::<_, std::io::Error>(Bytes::from("x".repeat(512)))));
    let response = tokio::time::timeout(
        Duration::from_secs(5),
        reqwest::Client::new()
            .post(&endpoint)
            .bearer_auth("secret")
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json")
            .header("mcp-protocol-version", "2025-11-25")
            .body(reqwest::Body::wrap_stream(chunks))
            .send(),
    )
    .await
    .expect("bounded chunked upload")
    .unwrap();
    assert_eq!(response.status(), 413);
    // Nothing was created for it: the cap is checked before a session exists.
    assert!(
        response.headers().get("mcp-session-id").is_none(),
        "a refused body must not mint a session"
    );
    drop(response);
    assert_eq!(host.entered.load(Ordering::SeqCst), 0);
    server.close().await;
    handle.abort();
}
