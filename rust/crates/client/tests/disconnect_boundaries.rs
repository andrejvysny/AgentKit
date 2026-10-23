use agentkit_client::{Client, StreamOptions};
use axum::{Router, body::Body, extract::State, response::Response, routing::get};
use futures::StreamExt;
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

struct Wire {
    bytes: Vec<u8>,
    cut: AtomicUsize,
    requests: AtomicUsize,
}

async fn stream(State(wire): State<Arc<Wire>>) -> Response {
    let first = wire.requests.fetch_add(1, Ordering::SeqCst) == 0;
    let bytes = if first {
        wire.bytes[..wire.cut.load(Ordering::SeqCst)].to_vec()
    } else {
        wire.bytes.clone()
    };
    Response::builder()
        .header("content-type", "text/event-stream")
        .body(Body::from(bytes))
        .unwrap()
}

#[tokio::test]
async fn disconnect_at_every_byte_replays_only_complete_events_until_settlement() {
    let mut bytes = String::new();
    for seq in 0..2 {
        let event = json!({"type":"run.message.delta","runId":"r","seq":seq,"eventId":format!("e{seq}"),"timestamp":"2026-01-01T00:00:00Z","contractVersion":"0.7.0","data":{"delta":"Ž🙂"}});
        bytes.push_str(&format!(
            "id: e{seq}\r\nevent: run.message.delta\r\ndata: {event}\r\n\r\n"
        ));
    }
    bytes.push_str("event: agentkit.stream.settled\r\ndata: {}\r\n\r\n");
    let wire = Arc::new(Wire {
        bytes: bytes.into_bytes(),
        cut: AtomicUsize::new(0),
        requests: AtomicUsize::new(0),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = Client::new(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let server = tokio::spawn(
        axum::serve(
            listener,
            Router::new()
                .route("/v1/runs/r/stream", get(stream))
                .with_state(wire.clone()),
        )
        .into_future(),
    );
    for cut in 0..wire.bytes.len() {
        wire.cut.store(cut, Ordering::SeqCst);
        wire.requests.store(0, Ordering::SeqCst);
        let options = StreamOptions {
            max_retries: 1,
            retry_delay: Duration::from_millis(0),
            ..Default::default()
        };
        let events: Vec<_> = client.stream_run("r", options).collect().await;
        assert_eq!(events.len(), 2, "cut {cut}: {events:?}");
        for (seq, event) in events.into_iter().enumerate() {
            let event = event.unwrap_or_else(|error| panic!("cut {cut}: {error}"));
            assert_eq!(event["seq"], seq);
            assert_eq!(event["data"]["delta"], "Ž🙂");
        }
    }
    server.abort();
}
