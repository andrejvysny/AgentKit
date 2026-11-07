//! AGENTKIT-80: the client MUST tell a server it abandoned a request.
//!
//! When a `tools/call` times out or its cancellation token fires, the JSON-RPC
//! `notifications/cancelled` (spec shape: `jsonrpc`, `method`,
//! `params.requestId`) has to travel on the request channel to the server —
//! otherwise the server keeps executing an effect nobody is waiting for and
//! the "delivery may be ambiguous" error the client reports is a lie by
//! omission: the server was never even told to stop. The hand-rolled capture
//! fixture records every line verbatim, so these assertions are wire-level
//! evidence, not a peer library's interpretation.

use agentkit_mcp::{ClientOptions, McpClient, McpClientError, model::JsonObject};
use axum::response::IntoResponse;
use serde_json::Value;
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

const TOOL: &str = "hang";

fn environment(capture: &std::path::Path) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("CAPTURE_FILE".into(), capture.to_str().unwrap().to_owned()),
        ("PATH".into(), std::env::var("PATH").unwrap()),
    ])
}

fn client_options(request_timeout: Duration) -> ClientOptions {
    ClientOptions {
        connect_timeout: Duration::from_secs(10),
        request_timeout,
        ..ClientOptions::default()
    }
}

async fn connect(capture: &std::path::Path, request_timeout: Duration) -> McpClient {
    let fixture = format!(
        "{}/tests/fixtures/cancel-capture-stdio.mjs",
        env!("CARGO_MANIFEST_DIR")
    );
    McpClient::stdio(
        "bun",
        &[fixture],
        &environment(capture),
        client_options(request_timeout),
    )
    .await
    .expect("the capture fixture speaks initialize")
}

fn lines(capture: &std::path::Path) -> Vec<Value> {
    std::fs::read_to_string(capture)
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// The `id` of the recorded `tools/call`, once the fixture has seen one.
fn call_id(capture: &std::path::Path) -> Option<Value> {
    lines(capture)
        .into_iter()
        .find(|message| message["method"] == "tools/call")
        .map(|message| message["id"].clone())
}

/// Poll until the capture holds a spec-shaped `notifications/cancelled`
/// naming `request_id`, then return it. Fails (with the full capture) after
/// the bound so the missing frame is visible in the assertion output.
async fn cancelled_notification(capture: &std::path::Path, request_id: &Value) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let found = lines(capture).into_iter().find(|message| {
            message["method"] == "notifications/cancelled"
                && message["params"]["requestId"] == *request_id
        });
        if let Some(message) = found {
            // Spec shape: a JSON-RPC 2.0 NOTIFICATION — jsonrpc + method +
            // params.requestId, and no `id` (a notification answers nothing).
            assert_eq!(message["jsonrpc"], Value::String("2.0".into()));
            assert_eq!(
                message["method"],
                Value::String("notifications/cancelled".into())
            );
            assert!(
                message.get("id").is_none(),
                "a notification carries no id: {message}"
            );
            return message;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!(
                "no notifications/cancelled for request id {request_id}; capture held: {:?}",
                lines(capture)
            );
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[cfg(unix)]
#[tokio::test]
async fn a_timed_out_tools_call_sends_notifications_cancelled_to_the_server() {
    let directory = tempfile::tempdir().unwrap();
    let capture = directory.path().join("capture.jsonl");
    let client = connect(&capture, Duration::from_millis(300)).await;

    let failure = client
        .call_tool(TOOL, JsonObject::new(), CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(failure, McpClientError::Timeout);

    let id =
        call_id(&capture).expect("the tools/call reaches the fixture before the deadline fires");
    cancelled_notification(&capture, &id).await;
    client.close().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn an_aborted_tools_call_sends_notifications_cancelled_to_the_server() {
    let directory = tempfile::tempdir().unwrap();
    let capture = directory.path().join("capture.jsonl");
    // The client's own deadline is far away: ONLY the token can end this call.
    let client = connect(&capture, Duration::from_secs(30)).await;
    let token = CancellationToken::new();

    // Wait until the call is on the wire, then pull the plug.
    let canceller = {
        let token = token.clone();
        let capture = capture.clone();
        tokio::spawn(async move {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            while tokio::time::Instant::now() < deadline {
                if call_id(&capture).is_some() {
                    token.cancel();
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
    };
    let failure = client
        .call_tool(TOOL, JsonObject::new(), token.clone())
        .await
        .unwrap_err();
    canceller.await.unwrap();
    assert_eq!(failure, McpClientError::Cancelled);

    let id = call_id(&capture).expect("the tools/call reached the fixture");
    cancelled_notification(&capture, &id).await;
    client.close().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn an_outer_deadline_drop_still_notifies_the_server() {
    // The manager's `bounded` deadline routinely drops the call future while
    // the client's own `request_timeout` is still pending — the abandonment
    // then happens in `Pending::drop`, which must still put the notification
    // on the wire best-effort.
    let directory = tempfile::tempdir().unwrap();
    let capture = directory.path().join("capture.jsonl");
    let client = connect(&capture, Duration::from_secs(30)).await;
    let outcome = tokio::time::timeout(
        Duration::from_millis(300),
        client.call_tool(TOOL, JsonObject::new(), CancellationToken::new()),
    )
    .await;
    assert!(outcome.is_err(), "the outer deadline must fire first");
    let id = call_id(&capture).expect("the tools/call reached the fixture");
    cancelled_notification(&capture, &id).await;
    client.close().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn a_dead_response_channel_still_ends_the_call_without_hanging() {
    // The fixture records the call and then exits: the pending response
    // channel is DEAD while the abandonment happens. Best-effort means the
    // undeliverable notification cannot hang or fake a cancellation — the
    // call must still settle promptly with its honest ambiguous outcome.
    let directory = tempfile::tempdir().unwrap();
    let capture = directory.path().join("capture.jsonl");
    let fixture = format!(
        "{}/tests/fixtures/cancel-capture-stdio.mjs",
        env!("CARGO_MANIFEST_DIR")
    );
    let mut environment = environment(&capture);
    environment.insert("EXIT_ON_CALL".into(), "1".into());
    let client = McpClient::stdio(
        "bun",
        &[fixture],
        &environment,
        client_options(Duration::from_millis(300)),
    )
    .await
    .expect("the capture fixture speaks initialize");

    let outcome = tokio::time::timeout(Duration::from_secs(10), async {
        client
            .call_tool(TOOL, JsonObject::new(), CancellationToken::new())
            .await
    })
    .await
    .expect("a dead response channel cannot strand the caller");
    assert!(matches!(
        outcome.unwrap_err(),
        McpClientError::Disconnected | McpClientError::Timeout
    ));
    // The abandoned request was at least seen by the server before it died.
    assert!(call_id(&capture).is_some());
}

// -- the HTTP lane: request channel (POST) alive, response channel (GET SSE)
//    dead. A streamable-HTTP server may accept `tools/call` with 202 and hold
//    the response for the server→client stream; when that stream is gone the
//    cancellation notification still MUST travel best-effort on the request
//    channel — otherwise the abandoned effect is never even disowned.

#[derive(Clone)]
struct HttpFake {
    posts: Arc<std::sync::Mutex<Vec<Value>>>,
    die_on_get: bool,
}

impl HttpFake {
    fn new(die_on_get: bool) -> Self {
        Self {
            posts: Arc::new(std::sync::Mutex::new(Vec::new())),
            die_on_get,
        }
    }
    fn recorded(&self) -> Vec<Value> {
        self.posts.lock().unwrap().clone()
    }
}

async fn serve_http(fake: HttpFake) -> (String, tokio::task::JoinHandle<()>) {
    let app = axum::Router::new()
        .route(
            "/mcp",
            axum::routing::post({
                let fake = fake.clone();
                move |body: axum::body::Bytes| {
                    let fake = fake.clone();
                    async move {
                        let message: Value =
                            serde_json::from_slice(&body).expect("a JSON-RPC frame");
                        let is_initialize = message["method"] == "initialize";
                        let id = message.get("id").cloned().unwrap_or(Value::Null);
                        fake.posts.lock().unwrap().push(message);
                        let body = if is_initialize {
                            axum::Json(serde_json::json!({
                                "jsonrpc": "2.0", "id": id, "result": {
                                    "protocolVersion": "2025-11-25",
                                    "capabilities": {"tools": {}},
                                    "serverInfo": {"name": "cancel-capture-http", "version": "1"}
                                }
                            }))
                            .into_response()
                        } else {
                            // Notifications and the deliberately unanswered
                            // tools/call both settle as bare acceptances: the
                            // call's real response is held back forever,
                            // standing in for one that would ride the
                            // server→client stream.
                            (http::StatusCode::ACCEPTED, "").into_response()
                        };
                        ([("mcp-session-id", "s1".to_owned())], body)
                    }
                }
            }),
        )
        .route(
            "/mcp",
            axum::routing::get({
                let fake = fake.clone();
                move || async move {
                    let body = if fake.die_on_get {
                        axum::body::Body::from_stream(futures::stream::empty::<
                            Result<String, std::io::Error>,
                        >())
                    } else {
                        axum::body::Body::from_stream(futures::stream::pending::<
                            Result<String, std::io::Error>,
                        >())
                    };
                    http::Response::builder()
                        .status(200)
                        .header("content-type", "text/event-stream")
                        .header("mcp-session-id", "s1")
                        .body(body)
                        .unwrap()
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/mcp", listener.local_addr().unwrap());
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (endpoint, handle)
}

async fn http_call_ends_ambiguously(die_on_get: bool) -> HttpFake {
    let fake = HttpFake::new(die_on_get);
    let (endpoint, server) = serve_http(fake.clone()).await;
    let client = McpClient::http(
        &endpoint,
        None,
        ClientOptions {
            connect_timeout: Duration::from_secs(10),
            request_timeout: Duration::from_millis(300),
            ..ClientOptions::default()
        },
    )
    .await
    .expect("the fake answers initialize");
    let failure = client
        .call_tool(TOOL, JsonObject::new(), CancellationToken::new())
        .await
        .unwrap_err();
    assert!(
        matches!(
            failure,
            McpClientError::Timeout | McpClientError::Disconnected
        ),
        "the call must settle as an ambiguous outcome, got {failure:?}"
    );
    let id = fake
        .recorded()
        .into_iter()
        .find(|message| message["method"] == "tools/call")
        .map(|message| message["id"].clone())
        .expect("the tools/call reaches the fake");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let found = fake.recorded().into_iter().find(|message| {
            message["method"] == "notifications/cancelled" && message["params"]["requestId"] == id
        });
        if let Some(message) = found {
            assert_eq!(message["jsonrpc"], Value::String("2.0".into()));
            assert!(
                message.get("id").is_none(),
                "a notification carries no id: {message}"
            );
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!(
                "no notifications/cancelled for request id {id}; posts held: {:?}",
                fake.recorded()
            );
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let _ = client.close().await;
    server.abort();
    fake
}

#[tokio::test]
async fn a_timed_out_http_tools_call_notifies_with_the_response_stream_open() {
    http_call_ends_ambiguously(false).await;
}

#[tokio::test]
async fn a_timed_out_http_tools_call_notifies_when_the_response_channel_is_dead() {
    http_call_ends_ambiguously(true).await;
}
