use agentkit_mcp::{ClientOptions, McpClient, McpClientError};
use axum::{body::Body, response::IntoResponse};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio_util::sync::CancellationToken;

async fn peer(
    request: http::Request<Body>,
    newline: &str,
    oversized: bool,
) -> axum::response::Response {
    if request.method() != http::Method::POST {
        return http::StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let bytes = axum::body::to_bytes(request.into_body(), 4_194_304)
        .await
        .unwrap();
    let request: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    if request.get("id").is_none() {
        return http::StatusCode::ACCEPTED.into_response();
    }
    let result = if request["method"] == "initialize" {
        json!({"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"framing","version":"1"}})
    } else {
        json!({"tools":[]})
    };
    let reply = json!({"jsonrpc":"2.0","id":request["id"],"result":result});
    if request["method"] == "tools/list" {
        let comments = if oversized {
            format!(":{}{newline}{newline}", "padding".repeat(160_000))
        } else {
            format!(":{}{newline}{newline}", "padding".repeat(100)).repeat(2000)
        };
        let body = format!("{comments}data: {reply}{newline}{newline}");
        assert!(body.len() > 1_048_576);
        return ([("content-type", "text/event-stream")], body).into_response();
    }
    axum::Json(reply).into_response()
}
async fn server(
    newline: &'static str,
    oversized: bool,
) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let requests = Arc::new(AtomicUsize::new(0));
    let count = requests.clone();
    let app = axum::Router::new().route(
        "/",
        axum::routing::any(move |request| {
            count.fetch_add(1, Ordering::SeqCst);
            peer(request, newline, oversized)
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, requests, task)
}
#[tokio::test]
async fn many_small_cr_lf_and_crlf_sse_frames_do_not_share_one_size_budget() {
    for newline in ["\r", "\n", "\r\n"] {
        let (url, _, task) = server(newline, false).await;
        let client = McpClient::http(&url, None, ClientOptions::default())
            .await
            .unwrap();
        let tools = client.list_tools(CancellationToken::new()).await;
        assert!(tools.is_ok(), "newline {newline:?}: {tools:?}");
        assert!(tools.unwrap().is_empty());
        client.close().await.unwrap();
        task.abort();
    }
}
#[tokio::test]
async fn reserved_last_event_id_is_rejected_before_any_network_request() {
    let (url, count, task) = server("\n", false).await;
    let mut headers = http::HeaderMap::new();
    headers.insert(
        "LaSt-EvEnT-Id",
        http::HeaderValue::from_static("caller-value"),
    );
    let result = McpClient::http_with_headers(&url, None, headers, ClientOptions::default()).await;
    assert!(matches!(result, Err(McpClientError::Configuration)));
    assert_eq!(count.load(Ordering::SeqCst), 0);
    task.abort();
}

#[tokio::test]
async fn one_oversized_sse_frame_is_rejected_for_every_line_ending() {
    for newline in ["\r", "\n", "\r\n"] {
        let (url, _, task) = server(newline, true).await;
        let client = McpClient::http(&url, None, ClientOptions::default())
            .await
            .unwrap();
        assert!(matches!(
            client.list_tools(CancellationToken::new()).await,
            Err(McpClientError::Disconnected)
        ));
        client.close().await.unwrap();
        task.abort();
    }
}
