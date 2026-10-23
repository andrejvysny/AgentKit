use agentkit_transport_http::{
    Actor, Backend, Config, OperationRequest, OperationResponse, Problem, router, routes,
};
use async_trait::async_trait;
use axum::http::HeaderMap;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

#[derive(Default)]
struct Fake {
    calls: Mutex<Vec<OperationRequest>>,
    keys: Mutex<HashMap<String, (String, Value)>>,
    log: Mutex<Vec<Value>>,
    settled: AtomicBool,
    event_reads: AtomicUsize,
    fail_after: AtomicUsize,
}
#[async_trait]
impl Backend for Fake {
    async fn authenticate(&self, headers: &HeaderMap) -> Result<Actor, Problem> {
        let id = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| Problem::new(401, "unauthorized", "Missing identity"))?;
        Ok(Actor {
            id: id.into(),
            metadata: BTreeMap::new(),
        })
    }
    async fn authorize(&self, input: &OperationRequest) -> Result<(), Problem> {
        if input.params.values().any(|id| id == "other") {
            Err(Problem::new(
                403,
                "forbidden",
                "Actor cannot access this resource",
            ))
        } else {
            Ok(())
        }
    }
    async fn dispatch(&self, input: OperationRequest) -> Result<OperationResponse, Problem> {
        self.calls.lock().unwrap().push(input.clone());
        if let Some(key) = &input.idempotency {
            let key_id = format!("{}:{}", input.actor.id, key.task_id);
            let mut keys = self.keys.lock().unwrap();
            if let Some((fingerprint, body)) = keys.get(&key_id) {
                if fingerprint != &key.fingerprint {
                    return Err(Problem::new(
                        422,
                        "idempotency_key_mismatch",
                        "Different request",
                    ));
                }
                return Ok(OperationResponse::json(200, body.clone()));
            }
            let body = json!({"runId":key.task_id,"userMessageId":"u","assistantMessageId":"a"});
            keys.insert(key_id, (key.fingerprint.clone(), body.clone()));
            return Ok(OperationResponse::json(201, body));
        }
        let status = match input.operation.as_str() {
            "createChat"
            | "createProvider"
            | "createMcpServer"
            | "grantAllowance"
            | "forkChat"
            | "publishPromptTemplate" => 201,
            "deleteChat"
            | "deleteProvider"
            | "deleteMcpServer"
            | "revokeAllowance"
            | "deleteConnection"
            | "deletePromptTemplate" => 204,
            "cancelRun" | "resumeRun" => 202,
            _ => 200,
        };
        Ok(OperationResponse::json(
            status,
            json!({"operation":input.operation,"actor":input.actor.id}),
        ))
    }
    async fn events(
        &self,
        _: &Actor,
        _: &str,
        after: Option<f64>,
        limit: usize,
    ) -> Result<Vec<Value>, Problem> {
        let reads = self.event_reads.fetch_add(1, Ordering::SeqCst);
        if self.fail_after.load(Ordering::SeqCst) > 0
            && reads >= self.fail_after.load(Ordering::SeqCst)
        {
            return Err(Problem::host("internal_error", "private storage password"));
        }
        Ok(self
            .log
            .lock()
            .unwrap()
            .iter()
            .filter(|e| {
                after.is_none_or(|s| agentkit_contracts::javascript_number(&e["seq"]).unwrap() > s)
            })
            .take(limit)
            .cloned()
            .collect())
    }
    async fn run_status(&self, _: &Actor, _: &str) -> Result<Option<String>, Problem> {
        Ok(Some(
            if self.settled.load(Ordering::SeqCst) {
                "completed"
            } else {
                "running"
            }
            .into(),
        ))
    }
}

async fn serve(fake: Arc<Fake>, config: Config) -> (String, tokio::task::JoinHandle<()>) {
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = socket.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(socket, router(fake, config)).await.unwrap();
    });
    (format!("http://{address}"), task)
}
fn event(seq: u64) -> Value {
    json!({"seq":seq,"eventId":format!("e{seq}"),"type":"run.message.delta","runId":"r",
        "timestamp":"2026-01-01T00:00:00Z","contractVersion":"0.7.0","data":{"delta":format!("{seq}")}})
}
fn body(operation: &str) -> Value {
    match operation {
        "submitMessage" => json!({"content":"hello"}),
        "forkChat" => json!({"fromMessageId":"m"}),
        "applyProposal" => json!({"operationId":"o"}),
        "createProvider" => {
            json!({"label":"Local","kind":"openai","baseUrl":"http://localhost","defaultModel":"m"})
        }
        "grantAllowance" => json!({"toolName":"t","proposalKind":"edit","maxRisk":"low"}),
        "createMcpServer" => json!({"alias":"tools","transport":{"kind":"stdio","command":"test"}}),
        _ => json!({}),
    }
}

#[tokio::test]
async fn all_48_routes_reach_backend_and_preserve_status() {
    let fake = Arc::new(Fake::default());
    fake.settled.store(true, Ordering::SeqCst);
    let (base, task) = serve(fake.clone(), Config::default()).await;
    let client = reqwest::Client::new();
    assert_eq!(routes().len(), 48);
    for route in routes() {
        let path = route
            .path
            .split('/')
            .map(|s| if s.starts_with(':') { "id" } else { s })
            .collect::<Vec<_>>()
            .join("/");
        let path = if route.operation == "searchMessages" {
            format!("{path}?q=hello")
        } else {
            path
        };
        let response = client
            .request(route.method.parse().unwrap(), format!("{base}{path}"))
            .header("authorization", "actor-one")
            .header("idempotency-key", "key")
            .json(&body(&route.operation))
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        let text = response.text().await.unwrap();
        assert!(status < 300, "{}: {status}: {text}", route.operation);
        if route.operation.starts_with("delete") || route.operation == "revokeAllowance" {
            assert_eq!(status, 204);
            assert!(text.is_empty());
        }
        if route.operation == "cancelRun" || route.operation == "resumeRun" {
            assert_eq!(status, 202);
        }
    }
    assert_eq!(fake.calls.lock().unwrap().len(), routes().len() - 1);
    task.abort();
}

#[tokio::test]
async fn boundaries_authorization_and_idempotency_over_real_socket() {
    let fake = Arc::new(Fake::default());
    let (base, task) = serve(
        fake.clone(),
        Config {
            max_body_bytes: 256,
            ..Config::default()
        },
    )
    .await;
    let client = reqwest::Client::new();
    for (path, status, code) in [
        ("/unknown", 404, "not_found"),
        ("/v1/chats/other", 403, "forbidden"),
        ("/v1/chats?limit=1001", 400, "invalid_request"),
    ] {
        let response = client
            .get(format!("{base}{path}"))
            .header("authorization", "one")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), status);
        assert_eq!(
            response.headers()["content-type"],
            "application/problem+json"
        );
        assert_eq!(response.json::<Value>().await.unwrap()["code"], code);
    }
    let response = client.put(format!("{base}/v1/chats")).send().await.unwrap();
    assert_eq!(response.status(), 405);
    assert!(response.headers().contains_key("allow"));
    let request = || {
        client
            .post(format!("{base}/v1/chats/c/messages"))
            .header("authorization", "one")
    };
    assert_eq!(
        request()
            .json(&json!({"content":"a"}))
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap()["code"],
        "idempotency_key_required"
    );
    assert_eq!(
        request()
            .body("x".repeat(257))
            .send()
            .await
            .unwrap()
            .status(),
        413
    );
    let first = request()
        .header("idempotency-key", "k")
        .json(&json!({"content":"a"}))
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), 201);
    let first = first.json::<Value>().await.unwrap();
    let replay = request()
        .header("idempotency-key", "k")
        .json(&json!({"content":"a"}))
        .send()
        .await
        .unwrap();
    assert_eq!(replay.status(), 200);
    assert_eq!(replay.json::<Value>().await.unwrap(), first);
    let changed = request()
        .header("idempotency-key", "k")
        .json(&json!({"content":"b"}))
        .send()
        .await
        .unwrap();
    assert_eq!(changed.status(), 422);
    assert_eq!(
        changed.json::<Value>().await.unwrap()["code"],
        "idempotency_key_mismatch"
    );
    task.abort();
}

#[tokio::test]
async fn replay_unknown_cursor_and_final_drain() {
    let fake = Arc::new(Fake::default());
    *fake.log.lock().unwrap() = (0..5).map(event).collect();
    fake.settled.store(true, Ordering::SeqCst);
    let mut config = Config::default();
    config.streaming.read_batch_size = 2;
    let (base, task) = serve(fake.clone(), config).await;
    let client = reqwest::Client::new();
    for (cursor, ids) in [
        ("unknown", vec!["e0", "e1", "e2", "e3", "e4"]),
        ("e2", vec!["e3", "e4"]),
    ] {
        let response = client
            .get(format!("{base}/v1/runs/r/stream"))
            .header("authorization", "one")
            .header("last-event-id", cursor)
            .send()
            .await
            .unwrap();
        let text = response.text().await.unwrap();
        let actual: Vec<_> = text
            .lines()
            .filter_map(|l| l.strip_prefix("id: "))
            .collect();
        assert_eq!(actual, ids);
        assert!(text.ends_with("event: agentkit.stream.settled\ndata: {}\n\n"));
    }
    task.abort();
}

#[tokio::test]
async fn provider_terminal_does_not_settle_and_read_failure_cannot_settle() {
    let fake = Arc::new(Fake::default());
    let mut terminal = event(0);
    terminal["type"] = json!("run.completed");
    *fake.log.lock().unwrap() = vec![terminal];
    let (base, task) = serve(fake.clone(), Config::default()).await;
    let response = reqwest::Client::new()
        .get(format!("{base}/v1/runs/r/stream"))
        .header("authorization", "one")
        .send()
        .await
        .unwrap();
    let pending = tokio::spawn(async move { response.text().await });
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    assert!(!pending.is_finished());
    fake.log.lock().unwrap().push(event(1));
    fake.settled.store(true, Ordering::SeqCst);
    let text = pending.await.unwrap().unwrap();
    assert!(text.contains("id: e1\n"));
    assert!(text.contains("agentkit.stream.settled"));
    task.abort();
    let fake = Arc::new(Fake::default());
    fake.fail_after.store(1, Ordering::SeqCst);
    *fake.log.lock().unwrap() = vec![event(0)];
    let (base, task) = serve(fake, Config::default()).await;
    let result = reqwest::Client::new()
        .get(format!("{base}/v1/runs/r/stream"))
        .header("authorization", "one")
        .send()
        .await;
    if let Ok(response) = result {
        assert!(response.text().await.is_err());
    }
    task.abort();
}

#[tokio::test]
async fn slow_body_consumer_reads_only_one_bounded_durable_page() {
    use futures::StreamExt;
    use tower::ServiceExt;
    let fake = Arc::new(Fake::default());
    *fake.log.lock().unwrap() = (0..1000).map(event).collect();
    let mut config = Config::default();
    config.streaming.read_batch_size = 2;
    let request = axum::http::Request::builder()
        .uri("/v1/runs/r/stream")
        .header("authorization", "one")
        .body(axum::body::Body::empty())
        .unwrap();
    let response = router(fake.clone(), config).oneshot(request).await.unwrap();
    let mut body = response.into_body().into_data_stream();
    body.next().await.unwrap().unwrap();
    body.next().await.unwrap().unwrap();
    let reads = fake.event_reads.load(Ordering::SeqCst);
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    assert_eq!(reads, 1);
    assert_eq!(fake.event_reads.load(Ordering::SeqCst), 1);
    drop(body);
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    assert_eq!(fake.event_reads.load(Ordering::SeqCst), 1);
}

#[test]
fn actor_metadata_preserves_arbitrary_objects_and_javascript_numbers() {
    let actor: Actor = serde_json::from_str(r#"{"id":"actor","metadata":{"marker":{"$serde_json::private::Number":"123"},"overflow":1e400,"rounded":9007199254740993}}"#).unwrap();
    assert!(actor.metadata["marker"].is_object());
    assert_eq!(
        actor.metadata["marker"]["$serde_json::private::Number"],
        "123"
    );
    assert_eq!(
        agentkit_contracts::javascript_number(&actor.metadata["overflow"]),
        Some(f64::INFINITY)
    );
    assert_eq!(
        agentkit_contracts::javascript_number(&actor.metadata["rounded"]),
        Some(9_007_199_254_740_992.0)
    );
}

#[tokio::test]
async fn numeric_cursor_preserves_gaps_fractions_negatives_and_wide_sequences() {
    let fake = Arc::new(Fake::default());
    let sequences = [
        -0.5,
        0.25,
        0.5,
        4.75,
        9_007_199_254_740_992.0,
        9_007_199_254_740_994.0,
        1e308,
    ];
    *fake.log.lock().unwrap() = sequences
        .into_iter()
        .enumerate()
        .map(|(index, seq)| {
            let mut value = event(0);
            value["seq"] = json!(seq);
            value["eventId"] = json!(format!("n{index}"));
            value
        })
        .collect();
    fake.settled.store(true, Ordering::SeqCst);
    let mut config = Config::default();
    config.streaming.read_batch_size = 1;
    let (base, task) = serve(fake, config).await;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
        .unwrap();
    for (cursor, expected) in [
        (None, 0),
        (Some("unknown"), 0),
        (Some("n2"), 3),
        (Some("n4"), 5),
        (Some("n6"), 7),
    ] {
        let mut request = client
            .get(format!("{base}/v1/runs/r/stream"))
            .header("authorization", "actor");
        if let Some(cursor) = cursor {
            request = request.header("last-event-id", cursor);
        }
        let wire = request.send().await.unwrap().text().await.unwrap();
        let ids: Vec<_> = wire
            .lines()
            .filter_map(|line| line.strip_prefix("id: "))
            .collect();
        assert_eq!(
            ids,
            (expected..7)
                .map(|index| format!("n{index}"))
                .collect::<Vec<_>>()
        );
        assert_eq!(wire.matches("event: agentkit.stream.settled").count(), 1);
    }
    task.abort();
}
