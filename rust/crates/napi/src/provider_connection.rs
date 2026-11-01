//! The Rust-native provider lane: a binding-local [`TrustedTransport`] over
//! reqwest plus the binding-instance connection registry.
//!
//! API keys never cross the JSON boundary: `registerProviderConnection` is
//! the only door a key travels through, and the key stays inside Rust
//! memory. `providerConfig` carries only the connection id, so the options
//! blob may be logged or persisted without ever leaking credentials. URL
//! joining and header rules mirror the TypeScript reference client
//! (`packages/core/src/providers/openai-compatible.ts`) exactly: one
//! trailing slash stripped from the base URL, `/{operation}` appended,
//! `accept: application/json` always, `content-type: application/json` on
//! POST operations, and `authorization: Bearer` only when a key is present.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use agentkit_core::providers::{AuthKind, ConnectionIdentity, TrustedTransport};
use agentkit_core::{CancellationToken, ChatProvider, ChatRequest, CoreError, ProviderStream};
use async_trait::async_trait;
use napi::bindgen_prelude::*;
use serde_json::Value;
use tokio::sync::Mutex;

use crate::errors;

/// A registered provider connection. This map entry is the only place a
/// key exists after registration; nothing else clones it across the
/// boundary. `generation` is the connection's fence counter: for keyed
/// (logical) registrations it mirrors the store-persisted generation set
/// by `agentkit_store::register_provider_connection` — stable across
/// restarts, advanced only by credential rotation — so the durable
/// continuation scope binds `{logical identity, generation}` rather than
/// a process-local accident.
#[derive(Clone)]
struct Connection {
    base_url: String,
    api_key: Option<String>,
    generation: u64,
}

/// Binding-instance connection registry, keyed by connection id (the same
/// global-routing pattern the callback bridge uses for in-flight calls):
/// exactly one registration door, and every request resolves through the
/// same map — revoking a connection fails the next request honestly with
/// `connection_changed` instead of letting a stale key keep firing.
static CONNECTIONS: LazyLock<Mutex<HashMap<String, Connection>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Register a provider connection: `baseUrl` addresses the OpenAI-compatible
/// server (the transport appends `/chat/completions`, `/responses`, or
/// `/models`), and `apiKey` — when present — is presented as
/// `Authorization: Bearer`. The key crosses the boundary exactly here and
/// never again: only the returned connection id may appear in options JSON.
///
/// Without `optionsJson` the id is a process-local random: correct for a
/// scratch lane, but a Responses continuation bound to it dies with the
/// process. For a lane whose continuation must survive restarts, pass
/// `{connectionKey, storePath}` — the durable LOGICAL identity door
/// (P1-E): the id is derived from the key alone (never from the secret),
/// the store persists the credential fingerprint + generation, and the
/// credential-restoration rules hold: same key + same credential on every
/// restart → the same id AND generation (the continuation keeps working);
/// same key + a different credential → a rotation (new generation under
/// the same identity, stored scopes restamped, continuation continues);
/// a wrong credential is the provider's verdict — an auth error at
/// request time, never a silent identity change.
#[napi]
pub async fn register_provider_connection(
    base_url: String,
    api_key: Option<String>,
    options_json: Option<String>,
) -> Result<String> {
    if base_url.is_empty() {
        return Err(errors::invalid("baseUrl must be a non-empty string"));
    }
    // An empty key is an absent key — the reference client sends no
    // `authorization` header for a falsy apiKey, and this lane must not
    // send a bare `Bearer `.
    let api_key = api_key.filter(|key| !key.is_empty());
    let (connection_id, generation) = match options_json.as_deref() {
        None | Some("") => (format!("conn_{}", uuid::Uuid::new_v4()), 1),
        Some(text) => {
            let options: Value = agentkit_contracts::parse_javascript_json(text)
                .map_err(|_| errors::invalid("provider connection options must be valid JSON"))?;
            let field = |key: &str| -> Result<String> {
                options
                    .get(key)
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned)
                    .ok_or_else(|| {
                        errors::invalid(format!(
                            "provider connection options.{key} must be a non-empty string"
                        ))
                    })
            };
            let connection_key = field("connectionKey")?;
            let store_path = field("storePath")?;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_millis() as i64)
                .unwrap_or(0);
            agentkit_store::register_provider_connection(
                std::path::Path::new(&store_path),
                &connection_key,
                api_key.as_deref(),
                now,
            )
            .map_err(errors::store)?
        }
    };
    CONNECTIONS.lock().await.insert(
        connection_id.clone(),
        Connection {
            base_url,
            api_key,
            generation,
        },
    );
    Ok(connection_id)
}

/// Revoke a connection. Returns false for an unknown or already-revoked id.
/// A run in flight observes the revocation as `connection_changed` at its
/// next request; no further HTTP leaves for that connection afterwards.
#[napi]
pub async fn revoke_provider_connection(connection_id: String) -> Result<bool> {
    Ok(CONNECTIONS.lock().await.remove(&connection_id).is_some())
}

/// The revocable-state fence `check_current` consults: the same
/// `Open → Closing → Closed` lifecycle that makes new JS dispatches fail
/// fast, so a binding that has started closing never issues a fresh
/// provider request. Injected (rather than naming the bridge directly) so
/// the transport's fence semantics are testable without a Node host.
pub(crate) type Fence = Arc<dyn (Fn() -> std::result::Result<(), CoreError>) + Send + Sync>;

/// One service's transport: an immutable identity plus the shared client.
/// The connection's base URL and key are resolved per request so revocation
/// and rotation take effect immediately, never from a stale snapshot.
pub(crate) struct BindingTransport {
    identity: ConnectionIdentity,
    fence: Fence,
    client: reqwest::Client,
}

impl BindingTransport {
    /// Resolve a registered connection into the transport for one
    /// `providerConfig`. An unknown connection id fails construction
    /// honestly (`invalid_connection`) — a service never starts with a
    /// provider whose connection does not exist.
    pub(crate) async fn connect(
        provider_id: &str,
        protocol: &str,
        connection_id: &str,
        fence: Fence,
    ) -> Result<Arc<Self>> {
        if provider_id.is_empty() {
            return Err(errors::connection(
                "providerConfig.providerId must be non-empty",
            ));
        }
        let connection = CONNECTIONS
            .lock()
            .await
            .get(connection_id)
            .cloned()
            .ok_or_else(|| {
                errors::connection(format!(
                    "providerConfig.connectionId \"{connection_id}\" is not a registered connection"
                ))
            })?;
        let client = reqwest::Client::builder()
            .build()
            .map_err(|_| errors::internal("the HTTP client"))?;
        Ok(Arc::new(Self {
            identity: ConnectionIdentity {
                provider_id: provider_id.to_owned(),
                protocol: protocol.to_owned(),
                connection_id: connection_id.to_owned(),
                // The registration's generation — durable for keyed
                // connections, 1 for process-local ones. A service built
                // after a rotation carries the new generation; a stale
                // pre-rotation object is rejected at the next admission
                // (the host's scope comparison), never silently trusted.
                generation: connection.generation,
            },
            fence,
            client,
        }))
    }
}

#[async_trait]
impl TrustedTransport for BindingTransport {
    fn check_current(&self) -> std::result::Result<(), CoreError> {
        (self.fence)()
    }

    fn identity(&self) -> ConnectionIdentity {
        self.identity.clone()
    }

    /// This lane is API-key shaped only (no subscription account exists to
    /// impersonate), matching the reference client's key-or-none auth.
    fn auth_kind(&self) -> AuthKind {
        AuthKind::ApiKey
    }

    async fn request(
        &self,
        identity: &ConnectionIdentity,
        operation: &str,
        body: Option<Value>,
        cancellation: CancellationToken,
    ) -> std::result::Result<reqwest::Response, CoreError> {
        if cancellation.is_cancelled() {
            return Err(CoreError::new("cancelled", "aborted"));
        }
        if identity != &self.identity {
            return Err(CoreError::responses("connection_changed"));
        }
        let path = match operation {
            "chat/completions" | "responses" => operation,
            "models" => "models",
            _ => return Err(CoreError::responses("invalid_input")),
        };
        let connection = CONNECTIONS
            .lock()
            .await
            .get(&self.identity.connection_id)
            .cloned()
            .ok_or_else(|| CoreError::responses("connection_changed"))?;
        // `models` is a keyless GET in the reference client (no body, no
        // content type); the completion operations POST a JSON body.
        let get = path == "models";
        let mut request = if get {
            self.client.get(join_url(&connection.base_url, path))
        } else {
            self.client.post(join_url(&connection.base_url, path))
        };
        request = request.header("accept", "application/json");
        if let Some(key) = &connection.api_key {
            request = request.header("authorization", format!("Bearer {key}"));
        }
        if !get {
            request = request.header("content-type", "application/json");
        }
        if let Some(body) = body.filter(|_| !get) {
            request = request.json(&body);
        }
        request
            .send()
            .await
            .map_err(|_| CoreError::responses("transport_error"))
    }
}

/// Join `{baseUrl}/{operation}` the way the TypeScript reference client
/// does: exactly one trailing slash is stripped from the base, then the
/// operation path is appended with a single separator.
fn join_url(base_url: &str, operation: &str) -> String {
    let base = base_url.strip_suffix('/').unwrap_or(base_url);
    format!("{base}/{operation}")
}

/// Applies the service-level `temperature`/`maxTokens` as REQUEST DEFAULTS
/// through [`ChatProvider::apply_request_defaults`] — i.e. BEFORE the
/// host's context admission — and then delegates untouched. The host
/// builds the chat request without sampling parameters, so the configured
/// lane options reach the request here; admission then clamps the output
/// cap, and the admitted value is what goes on the wire (P1-F): the
/// transmitted cap is min(context cap, configured maxTokens), and no
/// layer after admission — including this one — may override token limits.
/// A request that already carries a value keeps it; the config supplies
/// defaults, never overrides.
pub(crate) struct ConfiguredProvider {
    inner: Arc<dyn ChatProvider>,
    temperature: Option<f64>,
    max_tokens: Option<u64>,
}

impl ConfiguredProvider {
    pub(crate) fn new(
        inner: Arc<dyn ChatProvider>,
        temperature: Option<f64>,
        max_tokens: Option<u64>,
    ) -> Self {
        Self {
            inner,
            temperature,
            max_tokens,
        }
    }
}

#[async_trait]
impl ChatProvider for ConfiguredProvider {
    fn allows_implicit_output_limit(&self) -> bool {
        self.inner.allows_implicit_output_limit()
    }
    fn apply_request_defaults(&self, input: &mut ChatRequest) {
        self.inner.apply_request_defaults(input);
        if input.temperature.is_none() {
            input.temperature = self.temperature;
        }
        if input.max_output_tokens.is_none() {
            input.max_output_tokens = self.max_tokens;
        }
    }
    fn supports_usage_cleanup(&self) -> bool {
        self.inner.supports_usage_cleanup()
    }
    fn connection_identity(&self) -> Option<ConnectionIdentity> {
        self.inner.connection_identity()
    }
    fn tracks_transport_requests(&self) -> bool {
        self.inner.tracks_transport_requests()
    }
    fn id(&self) -> &str {
        self.inner.id()
    }
    fn stream_chat(&self, input: ChatRequest, cancellation: CancellationToken) -> ProviderStream {
        // The admitted request IS the transmitted request: nothing here
        // may touch the token budget or sampling parameters the host
        // resolved before admission.
        self.inner.stream_chat(input, cancellation)
    }
    async fn list_models(
        &self,
        cancellation: CancellationToken,
    ) -> std::result::Result<Vec<Value>, CoreError> {
        self.inner.list_models(cancellation).await
    }
}

#[cfg(test)]
mod tests {
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

    fn transport(connection_id: &str) -> BindingTransport {
        BindingTransport {
            identity: identity(connection_id),
            fence: open_fence(),
            client: reqwest::Client::builder().build().unwrap(),
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
        let transport = transport("test-revoked");
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
            .request(
                &identity("test-fence"),
                "chat/completions",
                Some(serde_json::json!({})),
                token,
            )
            .await
            .expect_err("a cancelled run never dispatches");
        assert_eq!(error.code, "cancelled");

        let closed = transport("test-fence");
        assert!(closed.check_current().is_ok());
        let closed = BindingTransport {
            fence: Arc::new(|| Err(CoreError::new("cancelled", "the binding is closing"))),
            ..transport("test-fence")
        };
        let error = closed.check_current().expect_err("closing is current");
        assert_eq!(error.code, "cancelled");
    }

    #[tokio::test]
    async fn connect_refuses_unknown_connections_and_empty_provider_ids() {
        let error =
            match BindingTransport::connect("", "chat-completions", "nope", open_fence()).await {
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

    struct Capture {
        seen: std::sync::Mutex<Option<ChatRequest>>,
    }
    #[async_trait]
    impl ChatProvider for Capture {
        fn id(&self) -> &str {
            "capture"
        }
        fn stream_chat(
            &self,
            input: ChatRequest,
            _cancellation: CancellationToken,
        ) -> ProviderStream {
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
}
