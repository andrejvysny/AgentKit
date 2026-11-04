use agentkit_client::{Call, Client, ClientError, StreamOptions};
use agentkit_contracts::RestOperation;
use axum::{Router, body::Body, response::Response, routing::get};
use futures::StreamExt;
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

async fn serve(router: Router) -> (Client, tokio::task::JoinHandle<()>) {
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", socket.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(socket, router).await.unwrap();
    });
    (Client::new(&base).unwrap(), task)
}

#[tokio::test]
async fn silent_event_stream_is_dropped_after_the_idle_timeout_and_reconnected() {
    let connections = Arc::new(AtomicUsize::new(0));
    let counter = connections.clone();
    let app = Router::new().route(
        "/v1/runs/r/stream",
        get(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            async {
                // Headers go out, then nothing: no frames, no heartbeats.
                Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(Body::from_stream(futures::stream::pending::<
                        Result<axum::body::Bytes, std::io::Error>,
                    >()))
                    .unwrap()
            }
        }),
    );
    let (mut client, task) = serve(app).await;
    client.stream_idle_timeout = Duration::from_millis(100);
    let options = StreamOptions {
        max_retries: 1,
        retry_delay: Duration::from_millis(1),
        ..StreamOptions::default()
    };
    let outcome = tokio::time::timeout(
        Duration::from_secs(10),
        client.stream_run("r", options).collect::<Vec<_>>(),
    )
    .await
    .expect("an idle stream must not hang the client");
    let error = outcome.into_iter().last().unwrap().unwrap_err();
    assert!(
        matches!(&error, ClientError::Protocol(message) if message.contains("idle")),
        "{error:?}"
    );
    assert_eq!(
        connections.load(Ordering::SeqCst),
        2,
        "one initial connection and one reconnect"
    );
    task.abort();
}

#[tokio::test]
async fn invalid_utf8_response_is_a_protocol_error() {
    let app = Router::new().route(
        "/v1/version",
        get(|| async {
            Response::builder()
                .header("content-type", "application/json")
                .body(Body::from(vec![b'{', 0xff, 0xfe, b'}']))
                .unwrap()
        }),
    );
    let (client, task) = serve(app).await;
    let error = client
        .call(RestOperation::GetVersion, Call::default())
        .await
        .unwrap_err();
    assert!(matches!(error, ClientError::Protocol(_)), "{error:?}");
    task.abort();
}
