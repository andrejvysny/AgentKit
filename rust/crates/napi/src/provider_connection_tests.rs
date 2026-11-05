use super::*;

fn identity(connection_id: &str) -> ConnectionIdentity {
    ConnectionIdentity {
        provider_id: "configured".to_owned(),
        protocol: "chat-completions".to_owned(),
        connection_id: connection_id.to_owned(),
        generation: 1,
    }
}

fn open_fence() -> Fence {
    Arc::new(|| Ok(()))
}

async fn transport(connection_id: &str) -> BindingTransport {
    let base_url = CONNECTIONS
        .lock()
        .await
        .get(connection_id)
        .map(|connection| connection.base_url.clone())
        .unwrap_or_default();
    BindingTransport {
        identity: identity(connection_id),
        base_url,
        fence: open_fence(),
        client: reqwest::Client::builder().build().unwrap(),
        authority: CONNECTIONS
            .lock()
            .await
            .get(connection_id)
            .map(|c| c.authority.clone())
            .unwrap_or_else(|| Arc::new(AtomicBool::new(false))),
        store_path: None,
    }
}

async fn serve_canned(response: &'static str) -> (String, std::thread::JoinHandle<String>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let thread = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        use std::io::{Read, Write};
        // Read until the full header block has arrived; bodies here are
        // tiny and ride the same segment on loopback.
        let mut seen = Vec::new();
        let mut buffer = [0_u8; 8192];
        while !seen.windows(4).any(|window| window == b"\r\n\r\n") {
            let read = stream.read(&mut buffer).unwrap();
            if read == 0 {
                break;
            }
            seen.extend_from_slice(&buffer[..read]);
        }
        stream.write_all(response.as_bytes()).unwrap();
        stream.flush().unwrap();
        String::from_utf8_lossy(&seen).into_owned()
    });
    (endpoint, thread)
}

async fn registered(connection_id: &str, base_url: String, api_key: Option<String>) {
    CONNECTIONS.lock().await.insert(
        connection_id.to_owned(),
        Connection {
            base_url,
            api_key,
            generation: 1,
            store_path: None,
            scope_key: None,
            account_scope: String::new(),
            authority: Arc::new(AtomicBool::new(true)),
        },
    );
}

#[tokio::test]
async fn connect_carries_the_registrations_generation() {
    CONNECTIONS.lock().await.insert(
        "test-generation".to_owned(),
        Connection {
            base_url: "http://127.0.0.1:1/v1".to_owned(),
            api_key: None,
            generation: 4,
            store_path: None,
            scope_key: None,
            account_scope: String::new(),
            authority: Arc::new(AtomicBool::new(true)),
        },
    );
    let transport =
        BindingTransport::connect("configured", "responses", "test-generation", open_fence())
            .await
            .unwrap();
    // The scope the continuation binds comes from the registration,
    // not from a construction-time constant: a rotated connection
    // builds a service at the persisted generation, and a stale
    // pre-rotation object cannot silently keep the old fence.
    assert_eq!(transport.identity().generation, 4);
}

#[test]
fn joins_urls_with_the_reference_clients_trailing_slash_rule() {
    assert_eq!(
        join_url("http://127.0.0.1:9/v1/", "chat/completions"),
        "http://127.0.0.1:9/v1/chat/completions"
    );
    assert_eq!(
        join_url("http://127.0.0.1:9/v1", "chat/completions"),
        "http://127.0.0.1:9/v1/chat/completions"
    );
    assert_eq!(
        join_url("http://127.0.0.1:9/", "models"),
        "http://127.0.0.1:9/models"
    );
    assert_eq!(
        join_url("http://127.0.0.1:9", "responses"),
        "http://127.0.0.1:9/responses"
    );
}

#[tokio::test]
async fn chat_completions_post_carries_bearer_key_and_json_headers() {
    let (endpoint, server) = serve_canned(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 11\r\n\r\n{\"ok\":true}",
    )
    .await;
    registered(
        "test-keyed",
        format!("{endpoint}/v1/"),
        Some("sk-secret".to_owned()),
    )
    .await;
    let response = transport("test-keyed")
        .await
        .request(
            &identity("test-keyed"),
            "chat/completions",
            Some(serde_json::json!({"model": "m"})),
            CancellationToken::new(),
        )
        .await
        .expect("request succeeds");
    assert!(response.status().is_success());
    assert_eq!(response.text().await.unwrap(), "{\"ok\":true}");
    let request = server.join().unwrap();
    assert!(
        request.starts_with("POST /v1/chat/completions HTTP/1.1"),
        "request line: {request}"
    );
    let lowered = request.to_ascii_lowercase();
    assert!(
        lowered.contains("authorization: bearer sk-secret"),
        "{request}"
    );
    assert!(lowered.contains("accept: application/json"), "{request}");
    assert!(
        lowered.contains("content-type: application/json"),
        "{request}"
    );
}

#[tokio::test]
async fn models_get_omits_body_content_type_and_keyless_auth() {
    let (endpoint, server) = serve_canned(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 11\r\n\r\n{\"ok\":true}",
    )
    .await;
    registered("test-keyless", format!("{endpoint}/v1"), None).await;
    let response = transport("test-keyless")
        .await
        .request(
            &identity("test-keyless"),
            "models",
            None,
            CancellationToken::new(),
        )
        .await
        .expect("request succeeds");
    assert!(response.status().is_success());
    let request = server.join().unwrap();
    assert!(
        request.starts_with("GET /v1/models HTTP/1.1"),
        "request line: {request}"
    );
    let lowered = request.to_ascii_lowercase();
    assert!(lowered.contains("accept: application/json"), "{request}");
    assert!(!lowered.contains("authorization:"), "{request}");
    assert!(!lowered.contains("content-type:"), "{request}");
}

#[tokio::test]
async fn unknown_operation_and_revoked_connection_fail_honestly() {
    registered("test-revoked", "http://127.0.0.1:1/v1".to_owned(), None).await;
    let transport = transport("test-revoked").await;
    let error = transport
        .request(
            &identity("test-revoked"),
            "chat/nope",
            None,
            CancellationToken::new(),
        )
        .await
        .expect_err("unknown operations are refused");
    assert_eq!(error.code, "invalid_input");
    assert!(
        CONNECTIONS.lock().await.remove("test-revoked").is_some(),
        "connection should exist before revoke"
    );
    let error = transport
        .request(
            &identity("test-revoked"),
            "chat/completions",
            Some(serde_json::json!({})),
            CancellationToken::new(),
        )
        .await
        .expect_err("a revoked connection never sends HTTP");
    assert_eq!(error.code, "connection_changed");
}

#[tokio::test]
async fn cancelled_token_and_closed_fence_refuse_work() {
    registered("test-fence", "http://127.0.0.1:1/v1".to_owned(), None).await;
    let token = CancellationToken::new();
    token.cancel();
    let error = transport("test-fence")
        .await
        .request(
            &identity("test-fence"),
            "chat/completions",
            Some(serde_json::json!({})),
            token,
        )
        .await
        .expect_err("a cancelled run never dispatches");
    assert_eq!(error.code, "cancelled");

    let closed = transport("test-fence").await;
    assert!(closed.check_current().is_ok());
    let closed = BindingTransport {
        fence: Arc::new(|| Err(CoreError::new("cancelled", "the binding is closing"))),
        ..transport("test-fence").await
    };
    let error = closed.check_current().expect_err("closing is current");
    assert_eq!(error.code, "cancelled");
}

#[tokio::test]
async fn connect_refuses_unknown_connections_and_empty_provider_ids() {
    let error = match BindingTransport::connect("", "chat-completions", "nope", open_fence()).await
    {
        Err(error) => error,
        Ok(_) => panic!("an empty providerId is refused"),
    };
    let payload: Value = serde_json::from_str(&error.reason).unwrap();
    assert_eq!(payload["code"], "invalid_connection");
    let error = match BindingTransport::connect(
        "configured",
        "chat-completions",
        "never-registered",
        open_fence(),
    )
    .await
    {
        Err(error) => error,
        Ok(_) => panic!("an unknown connectionId is refused"),
    };
    let payload: Value = serde_json::from_str(&error.reason).unwrap();
    assert_eq!(payload["code"], "invalid_connection");
}

#[test]
fn registration_requires_a_base_url_and_treats_empty_keys_as_absent() {
    // The napi wrappers are pure marshaling; assert their Rust halves
    // directly so the test binary never needs a Node host.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    runtime.block_on(async {
        let error = register_provider_connection(String::new(), None, None)
            .await
            .expect_err("an empty baseUrl is refused");
        let payload: Value = serde_json::from_str(&error.reason).unwrap();
        assert_eq!(payload["code"], "invalid_input");
        // A keyless registration stays the process-local random door…
        let connection_id = register_provider_connection(
            "https://api.test/v1".to_owned(),
            Some(String::new()),
            None,
        )
        .await
        .unwrap();
        assert!(
            revoke_provider_connection(connection_id.clone())
                .await
                .unwrap()
        );
        assert!(!revoke_provider_connection(connection_id).await.unwrap());
        // …and the keyed door is validated up front: both fields or
        // nothing, never a half-durable identity.
        for options in [
            "{",
            r#"{"connectionKey":"k"}"#,
            r#"{"storePath":"/tmp/never.sqlite"}"#,
            r#"{"connectionKey":"","storePath":"/tmp/never.sqlite"}"#,
            r#"{"connectionKey":"k","storePath":""}"#,
        ] {
            let error = register_provider_connection(
                "https://api.test/v1".to_owned(),
                Some("sk".to_owned()),
                Some(options.to_owned()),
            )
            .await
            .expect_err("malformed connection options are refused");
            let payload: Value = serde_json::from_str(&error.reason).unwrap();
            assert_eq!(payload["code"], "invalid_input", "{options}");
        }
    });
}

#[tokio::test]
async fn rotation_or_rebase_mid_run_yields_connection_changed() {
    registered("test-rotate", "http://127.0.0.1:1/v1".to_owned(), None).await;
    let transport = transport("test-rotate").await;
    // A rotation under the same id advances the generation.
    CONNECTIONS.lock().await.insert(
        "test-rotate".to_owned(),
        Connection {
            base_url: "http://127.0.0.1:1/v1".to_owned(),
            api_key: Some("new".to_owned()),
            generation: 2,
            store_path: None,
            scope_key: None,
            account_scope: String::new(),
            authority: Arc::new(AtomicBool::new(true)),
        },
    );
    let error = transport
        .request(
            &identity("test-rotate"),
            "chat/completions",
            Some(serde_json::json!({})),
            CancellationToken::new(),
        )
        .await
        .expect_err("a rotated generation never sends HTTP");
    assert_eq!(error.code, "connection_changed");
    // A swapped base URL under the same generation is refused as well.
    registered("test-rotate", "http://127.0.0.1:2/v1".to_owned(), None).await;
    let error = transport
        .request(
            &identity("test-rotate"),
            "chat/completions",
            Some(serde_json::json!({})),
            CancellationToken::new(),
        )
        .await
        .expect_err("a changed base url never sends HTTP");
    assert_eq!(error.code, "connection_changed");
}

struct Capture {
    seen: std::sync::Mutex<Option<ChatRequest>>,
}
#[async_trait]
impl ChatProvider for Capture {
    fn id(&self) -> &str {
        "capture"
    }
    fn stream_chat(&self, input: ChatRequest, _cancellation: CancellationToken) -> ProviderStream {
        *self.seen.lock().unwrap() = Some(input);
        Box::pin(futures::stream::iter(Vec::new()))
    }
}
#[test]
fn lane_config_supplies_defaults_before_admission_and_never_overrides() {
    let capture = Arc::new(Capture {
        seen: std::sync::Mutex::new(None),
    });
    let lane = ConfiguredProvider::new(capture.clone(), Some(0.25), Some(99));
    // The defaults land on a bare request BEFORE admission fills the
    // budget…
    let mut request = ChatRequest {
        model: "m".into(),
        ..Default::default()
    };
    lane.apply_request_defaults(&mut request);
    assert_eq!(request.temperature, Some(0.25));
    assert_eq!(request.max_output_tokens, Some(99));
    // …and a value the request already carries is kept (P1-F: the
    // config supplies defaults, never overrides).
    let mut carried = ChatRequest {
        model: "m".into(),
        temperature: Some(0.9),
        max_output_tokens: Some(16),
        ..Default::default()
    };
    lane.apply_request_defaults(&mut carried);
    assert_eq!(carried.temperature, Some(0.9));
    assert_eq!(carried.max_output_tokens, Some(16));
    // Nothing after admission may move them: a request whose cap the
    // host already clamped reaches the inner provider untouched.
    let _ = lane.stream_chat(carried.clone(), CancellationToken::new());
    let seen = capture.seen.lock().unwrap().clone().unwrap();
    assert_eq!(seen.temperature, Some(0.9));
    assert_eq!(seen.max_output_tokens, Some(16));
}
#[test]
fn lane_without_config_passes_request_through() {
    let capture = Arc::new(Capture {
        seen: std::sync::Mutex::new(None),
    });
    let lane = ConfiguredProvider::new(capture.clone(), None, None);
    let mut request = ChatRequest {
        model: "m".into(),
        temperature: Some(0.9),
        max_output_tokens: Some(16_384),
        ..Default::default()
    };
    lane.apply_request_defaults(&mut request);
    let _ = lane.stream_chat(request, CancellationToken::new());
    let seen = capture.seen.lock().unwrap().clone().unwrap();
    assert_eq!(seen.temperature, Some(0.9));
    assert_eq!(seen.max_output_tokens, Some(16_384));
}
