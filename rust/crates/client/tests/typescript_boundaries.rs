use agentkit_client::{Call, Client, RestOperation, StreamOptions};
use axum::{
    Router,
    body::Body,
    extract::{Path, State},
    http::HeaderMap,
    response::Response,
    routing::get,
};
use futures::{StreamExt, stream};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
    sync::Arc,
    time::Duration,
};
use tokio::sync::Mutex;

mod support;

struct Service(Child);
impl Drop for Service {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Proxy {
    bytes: Vec<u8>,
    upstream: String,
    seen: Mutex<HashSet<usize>>,
    http: reqwest::Client,
}

async fn replay(
    State(proxy): State<Arc<Proxy>>,
    Path((cut, run)): Path<(usize, String)>,
    headers: HeaderMap,
) -> Response {
    let bytes = if proxy.seen.lock().await.insert(cut) {
        proxy.bytes[..cut].to_vec()
    } else {
        let mut request = proxy
            .http
            .get(format!("{}/v1/runs/{run}/stream", proxy.upstream));
        if let Some(cursor) = headers.get("last-event-id") {
            request = request.header("last-event-id", cursor);
        }
        let response = request.send().await.unwrap();
        assert!(response.status().is_success());
        response.bytes().await.unwrap().to_vec()
    };
    Response::builder()
        .header("content-type", "text/event-stream")
        .body(Body::from(bytes))
        .unwrap()
}

async fn service() -> (Service, String) {
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/ts-service.ts");
    let child = Command::new("bun")
        .arg(script)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut service = Service(child);
    let stdout = service.0.stdout.take().unwrap();
    let base = tokio::task::spawn_blocking(move || {
        let mut line = String::new();
        BufReader::new(stdout).read_line(&mut line).unwrap();
        line.trim().trim_end_matches('/').to_owned()
    })
    .await
    .unwrap();
    (service, base)
}

async fn completed_turn(base: &str) -> (String, Vec<Value>) {
    let client = Client::new(base).unwrap();
    let chat = client
        .call(
            RestOperation::CreateChat,
            Call {
                body: Some(json!({})),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let turn = client
        .submit_message(
            chat["id"].as_str().unwrap(),
            json!({"content":"Ž🙂"}),
            "every-byte",
        )
        .await
        .unwrap();
    let run = turn["runId"].as_str().unwrap().to_owned();
    let expected: Vec<Value> = client
        .stream_run(&run, StreamOptions::default())
        .map(Result::unwrap)
        .collect()
        .await;
    assert!(!expected.is_empty());
    (run, expected)
}

async fn proxy(
    base: &str,
    run: &str,
) -> (
    std::net::SocketAddr,
    usize,
    tokio::task::JoinHandle<std::io::Result<()>>,
) {
    let http = reqwest::Client::new();
    let bytes = http
        .get(format!("{base}/v1/runs/{run}/stream"))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap()
        .to_vec();
    let count = bytes.len();
    assert!(count > 0);
    let proxy = Arc::new(Proxy {
        bytes,
        upstream: base.to_owned(),
        seen: Mutex::new(HashSet::new()),
        http,
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(
        axum::serve(
            listener,
            Router::new()
                .route("/{cut}/v1/runs/{run}/stream", get(replay))
                .with_state(proxy),
        )
        .into_future(),
    );
    (address, count, server)
}

#[tokio::test]
async fn native_client_resumes_typescript_sse_at_every_byte_boundary() {
    if support::ts_interop_skipped() {
        return;
    }
    let (_service, base) = service().await;
    let (run, expected) = completed_turn(&base).await;
    let (address, count, server) = proxy(&base, &run).await;
    let checks = stream::iter(0..count).for_each_concurrent(64, |cut| {
        let expected = &expected;
        let run = &run;
        async move {
            let client = Client::new(&format!("http://{address}/{cut}")).unwrap();
            let options = StreamOptions {
                max_retries: 1,
                max_total_reconnects: 1,
                retry_delay: Duration::ZERO,
                ..Default::default()
            };
            let actual: Vec<Value> = client
                .stream_run(run, options)
                .map(|event| event.unwrap_or_else(|error| panic!("cut {cut}: {error}")))
                .collect()
                .await;
            assert_eq!(&actual, expected, "cut {cut}");
        }
    });
    tokio::time::timeout(Duration::from_secs(180), checks)
        .await
        .unwrap();
    server.abort();
}
