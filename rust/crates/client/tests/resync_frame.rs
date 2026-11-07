//! AGENTKIT-81 (client leg): the `agentkit.stream.resync` control frame is
//! consumed, never yielded, and never mistaken for a durable run event. It
//! announces that a real `Last-Event-ID` was not recognised and the stream
//! replays the whole log — the events that follow still flow through the
//! normal validation and dedupe path.

use agentkit_client::{Client, ClientError, StreamOptions};
use axum::{Router, body::Body, response::Response, routing::get};
use futures::StreamExt;
use serde_json::{Value, json};
use std::time::Duration;

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
fn resync(reason: &str) -> String {
    format!("event: agentkit.stream.resync\ndata: {reason}\n\n")
}

async fn fixed(wire: String) -> Response {
    Response::builder()
        .header("content-type", "text/event-stream")
        .body(Body::from(wire))
        .unwrap()
}

async fn serve(wire: String) -> (Client, tokio::task::JoinHandle<()>) {
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", socket.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(
            socket,
            Router::new().route("/v1/runs/r/stream", get(move || fixed(wire.clone()))),
        )
        .await
        .unwrap();
    });
    (Client::new(&base).unwrap(), task)
}

fn options(last_event_id: &str) -> StreamOptions {
    StreamOptions {
        last_event_id: Some(last_event_id.into()),
        retry_delay: Duration::from_millis(1),
        ..StreamOptions::default()
    }
}

#[tokio::test]
async fn resync_frame_is_consumed_and_the_replayed_events_still_flow() {
    // What the server sends when the cursor was unknown: hint, resync,
    // the whole log, settlement.
    let wire = format!(
        "retry: 10\n\n{}{}{}{}event: agentkit.stream.settled\ndata: {{}}\n\n",
        resync("{\"reason\":\"unknown_last_event_id\"}"),
        frame(0),
        frame(1),
        frame(2),
    );
    let (client, task) = serve(wire).await;
    let events: Vec<_> = client
        .stream_run("r", options("from-another-run"))
        .map(|event| event.expect("the resync marker is not a protocol failure"))
        .collect()
        .await;
    assert_eq!(
        events
            .iter()
            .map(|event| event["seq"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        [0, 1, 2],
        "the frame announces the replay; it never becomes a client event"
    );
    assert!(
        !events
            .iter()
            .any(|event| event["type"] == "agentkit.stream.resync")
    );
    task.abort();
}

#[tokio::test]
async fn resync_frame_still_dedupes_against_an_earlier_high_water_mark() {
    // First connection ends without settlement (reconnect); the second one
    // announces a full replay. The dedupe by `seq` must drop the already
    // delivered e0 while e1/e2 still arrive.
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", socket.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        axum::serve(
            socket,
            Router::new().route(
                "/v1/runs/r/stream",
                get({
                    let calls = std::sync::Arc::clone(&calls);
                    move || {
                        let n = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        async move {
                            let wire = if n == 0 {
                                frame(0) // unmarked EOF: forces one reconnect
                            } else {
                                format!(
                                    "{}{}{}{}event: agentkit.stream.settled\ndata: {{}}\n\n",
                                    resync("{\"reason\":\"unknown_last_event_id\"}"),
                                    frame(0),
                                    frame(1),
                                    frame(2),
                                )
                            };
                            Response::builder()
                                .header("content-type", "text/event-stream")
                                .body(Body::from(wire))
                                .unwrap()
                        }
                    }
                }),
            ),
        )
        .await
        .unwrap();
    });
    let client = Client::new(&base).unwrap();
    let events: Vec<_> = client
        .stream_run("r", options("stale"))
        .map(|event| event.unwrap())
        .collect()
        .await;
    assert_eq!(
        events
            .iter()
            .map(|event| event["seq"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        [0, 1, 2],
        "e0 arrives once from the first connection; the announced replay of it is deduped"
    );
    task.abort();
}

#[tokio::test]
async fn a_malformed_resync_marker_is_a_protocol_failure() {
    let wire = format!(
        "{}{}event: agentkit.stream.settled\ndata: {{}}\n\n",
        resync("{}"),
        frame(0),
    );
    let (client, task) = serve(wire).await;
    let stream = client.stream_run("r", options("stale"));
    futures::pin_mut!(stream);
    let error = stream.next().await.expect("a result").unwrap_err();
    assert!(
        matches!(&error, ClientError::Protocol(message) if message == "Invalid resync marker"),
        "{error:?}"
    );
    task.abort();
}
