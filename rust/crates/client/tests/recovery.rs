use agentkit_client::{Client, ClientError, StreamOptions};
use axum::{
    Router,
    body::{Body, Bytes},
    extract::State,
    http::{HeaderMap, StatusCode},
    response::Response,
    routing::{get, post},
};
use futures::StreamExt;
use serde_json::{Value, json};
use std::{
    io,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

#[derive(Default)]
struct Server {
    count: AtomicUsize,
    keys: Mutex<Vec<String>>,
    cursors: Mutex<Vec<String>>,
}
fn event(seq: u64) -> Value {
    json!({"type":"run.message.delta","seq":seq,"eventId":format!("e{seq}"),"runId":"r",
        "contractVersion":"0.7.0","timestamp":"2026-01-01T00:00:00Z","data":{"delta":seq.to_string()}})
}
fn frame(seq: u64) -> String {
    format!(
        "id: e{seq}\nevent: run.message.delta\ndata: {}\n\n",
        event(seq)
    )
}
async fn sse(State(server): State<Arc<Server>>, headers: HeaderMap) -> Response {
    server.cursors.lock().unwrap().push(
        headers
            .get("last-event-id")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .into(),
    );
    let index = server.count.fetch_add(1, Ordering::SeqCst);
    let data = if index == 0 {
        frame(0)
    } else {
        format!(
            "{}{}event: agentkit.stream.settled\ndata: {{}}\n\n",
            frame(0),
            frame(1)
        )
    };
    Response::builder()
        .header("content-type", "text/event-stream")
        .body(Body::from(data))
        .unwrap()
}
async fn submit(State(server): State<Arc<Server>>, headers: HeaderMap) -> Response {
    server
        .keys
        .lock()
        .unwrap()
        .push(headers["idempotency-key"].to_str().unwrap().into());
    if server.count.fetch_add(1, Ordering::SeqCst) == 0 {
        let body = futures::stream::iter(vec![
            Ok(Bytes::from_static(b"{")),
            Err(io::Error::other("wire cut")),
        ]);
        Response::builder()
            .status(StatusCode::CREATED)
            .header("content-type", "application/json")
            .body(Body::from_stream(body))
            .unwrap()
    } else {
        Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/json")
            .body(Body::from(json!({"runId":"r"}).to_string()))
            .unwrap()
    }
}
async fn serve(router: Router) -> (Client, tokio::task::JoinHandle<()>) {
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", socket.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(socket, router).await.unwrap();
    });
    (Client::new(&base).unwrap(), task)
}

#[tokio::test]
async fn lost_response_retries_exact_same_idempotency_key() {
    let server = Arc::new(Server::default());
    let (client, task) = serve(
        Router::new()
            .route("/v1/chats/c/messages", post(submit))
            .with_state(server.clone()),
    )
    .await;
    assert_eq!(
        client
            .submit_message("c", json!({"content":"q"}), "stable-key")
            .await
            .unwrap()["runId"],
        "r"
    );
    assert_eq!(
        *server.keys.lock().unwrap(),
        vec!["stable-key", "stable-key"]
    );
    task.abort();
}

#[tokio::test]
async fn missing_settlement_reconnects_from_last_complete_frame_and_dedupes() {
    let server = Arc::new(Server::default());
    let (client, task) = serve(
        Router::new()
            .route("/v1/runs/r/stream", get(sse))
            .with_state(server.clone()),
    )
    .await;
    let options = StreamOptions {
        retry_delay: Duration::from_millis(1),
        ..StreamOptions::default()
    };
    let events: Vec<_> = client
        .stream_run("r", options)
        .map(|e| e.unwrap())
        .collect()
        .await;
    assert_eq!(events.len(), 2);
    assert_eq!(events[1]["seq"], 1);
    assert_eq!(*server.cursors.lock().unwrap(), vec!["", "e0"]);
    task.abort();
}

#[tokio::test]
async fn unmarked_eof_is_a_failure_even_after_gapped_progress() {
    for (wire, expected) in [
        (
            format!("{}{}", frame(0), frame(2)),
            "without a settlement marker",
        ),
        (frame(0), "without a settlement marker"),
    ] {
        let app = Router::new().route(
            "/v1/runs/r/stream",
            get(move || {
                let wire = wire.clone();
                async move {
                    Response::builder()
                        .header("content-type", "text/event-stream")
                        .body(Body::from(wire))
                        .unwrap()
                }
            }),
        );
        let (client, task) = serve(app).await;
        let result: Vec<_> = client
            .stream_run(
                "r",
                StreamOptions {
                    max_retries: 0,
                    ..StreamOptions::default()
                },
            )
            .collect()
            .await;
        assert!(
            matches!(result.last(),Some(Err(ClientError::Protocol(message))) if message.contains(expected)),
            "{result:?}"
        );
        task.abort();
    }
}

#[tokio::test]
async fn numeric_high_water_dedupes_replay_without_requiring_integer_steps() {
    let server = Arc::new(Server::default());
    let source = server.clone();
    let sequences = [
        -2.5,
        0.25,
        0.5,
        9_007_199_254_740_992.0,
        9_007_199_254_740_994.0,
        1e308,
    ];
    let app = Router::new().route(
        "/v1/runs/r/stream",
        get(move |headers: HeaderMap| {
            let server = source.clone();
            async move {
                server.cursors.lock().unwrap().push(
                    headers
                        .get("last-event-id")
                        .and_then(|h| h.to_str().ok())
                        .unwrap_or("")
                        .into(),
                );
                let first = server.count.fetch_add(1, Ordering::SeqCst) == 0;
                let mut wire = String::new();
                for (index, seq) in
                    sequences
                        .into_iter()
                        .enumerate()
                        .take(if first { 2 } else { 6 })
                {
                    let mut value = event(0);
                    value["seq"] = json!(seq);
                    value["eventId"] = json!(format!("n{index}"));
                    wire.push_str(&format!(
                        "id: n{index}\nevent: run.message.delta\ndata: {value}\n\n"
                    ));
                }
                if !first {
                    wire.push_str("event: agentkit.stream.settled\ndata: {}\n\n");
                }
                Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(Body::from(wire))
                    .unwrap()
            }
        }),
    );
    let (client, task) = serve(app).await;
    let events: Vec<_> = client
        .stream_run(
            "r",
            StreamOptions {
                retry_delay: Duration::from_millis(1),
                ..StreamOptions::default()
            },
        )
        .map(|event| event.unwrap())
        .collect()
        .await;
    assert_eq!(
        events
            .iter()
            .map(|event| agentkit_contracts::javascript_number(&event["seq"]).unwrap())
            .collect::<Vec<_>>(),
        sequences
    );
    assert_eq!(*server.cursors.lock().unwrap(), vec!["", "n1"]);
    task.abort();
}

#[tokio::test]
async fn empty_event_id_is_valid_and_replayed_numeric_progress_is_deduplicated() {
    let count = Arc::new(AtomicUsize::new(0));
    let cursors = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
    let captured = cursors.clone();
    let app = Router::new().route(
        "/v1/runs/r/stream",
        get(move |headers: HeaderMap| {
            let count = count.clone();
            let cursors = captured.clone();
            async move {
                cursors.lock().unwrap().push(
                    headers
                        .get("last-event-id")
                        .map(|value| value.to_str().unwrap().to_owned()),
                );
                let first = count.fetch_add(1, Ordering::SeqCst) == 0;
                let mut empty = event(0);
                empty["seq"] = json!(-0.5);
                empty["eventId"] = json!("");
                let wire = format!(
                    "id: \nevent: run.message.delta\ndata: {empty}\n\n{}",
                    if first {
                        String::new()
                    } else {
                        format!("{}event: agentkit.stream.settled\ndata: {{}}\n\n", frame(3))
                    }
                );
                Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(Body::from(wire))
                    .unwrap()
            }
        }),
    );
    let (client, task) = serve(app).await;
    let received: Vec<_> = client
        .stream_run(
            "r",
            StreamOptions {
                retry_delay: Duration::from_millis(1),
                ..StreamOptions::default()
            },
        )
        .map(|event| event.unwrap())
        .collect()
        .await;
    assert_eq!(
        received
            .iter()
            .map(|event| event["eventId"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["", "e3"]
    );
    assert_eq!(*cursors.lock().unwrap(), vec![None, Some(String::new())]);
    task.abort();
}
