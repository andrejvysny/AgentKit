use super::*;
use agentkit_core::{ProviderEvent, providers::ChatCompletionsProvider};
use futures::StreamExt;
use serde_json::json;

struct TestStore {
    path: std::path::PathBuf,
    directory: std::path::PathBuf,
}
impl TestStore {
    fn new() -> Self {
        let directory =
            std::env::temp_dir().join(format!("agentkit-provider-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("store.sqlite");
        drop(agentkit_store::SqliteStore::open(&path, 4).unwrap());
        Self { path, directory }
    }
    fn options(&self) -> String {
        json!({"connectionKey":"main","storePath":self.path,"accountScope":"account"}).to_string()
    }
}
impl Drop for TestStore {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}
fn open_fence() -> Fence {
    Arc::new(|| Ok(()))
}

#[tokio::test]
async fn independent_store_registrations_never_redirect_each_other() {
    let first = TestStore::new();
    let second = TestStore::new();
    let a = register_provider_connection(
        "http://127.0.0.1:1".into(),
        Some("a".into()),
        Some(first.options()),
    )
    .await
    .unwrap();
    let b = register_provider_connection(
        "http://127.0.0.1:2".into(),
        Some("b".into()),
        Some(second.options()),
    )
    .await
    .unwrap();
    assert_ne!(a, b);
    let transport = BindingTransport::connect("p", "responses", &a, open_fence())
        .await
        .unwrap();
    transport.check_authority().await.unwrap();
    let entry = CONNECTIONS.lock().await.get(&a).cloned().unwrap();
    assert_eq!(entry.api_key.as_deref(), Some("a"));
    assert_eq!(entry.base_url, "http://127.0.0.1:1");
}

#[tokio::test]
async fn revocation_and_identical_reregistration_do_not_revive_old_transports() {
    let store = TestStore::new();
    let a = register_provider_connection(
        "http://127.0.0.1:1".into(),
        Some("a".into()),
        Some(store.options()),
    )
    .await
    .unwrap();
    let transport = BindingTransport::connect("p", "responses", &a, open_fence())
        .await
        .unwrap();
    assert!(revoke_provider_connection(a.clone()).await.unwrap());
    assert_eq!(
        transport.check_current().unwrap_err().code,
        "connection_changed"
    );
    let b = register_provider_connection(
        "http://127.0.0.1:1".into(),
        Some("a".into()),
        Some(store.options()),
    )
    .await
    .unwrap();
    assert_eq!(a, b);
    let restored = BindingTransport::connect("p", "responses", &b, open_fence())
        .await
        .unwrap();
    assert_eq!(restored.identity().generation, 2);
    restored.check_authority().await.unwrap();
    assert_eq!(
        transport.check_authority().await.unwrap_err().code,
        "connection_changed"
    );
}

#[tokio::test]
async fn stale_before_dispatch_sends_zero_requests() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let store = TestStore::new();
    let id = register_provider_connection(url.clone(), Some("one".into()), Some(store.options()))
        .await
        .unwrap();
    let transport = BindingTransport::connect("p", "responses", &id, open_fence())
        .await
        .unwrap();
    agentkit_store::register_scoped_provider_connection(
        &store.path,
        "main",
        Some("two"),
        &url,
        "account",
        1000,
    )
    .unwrap();
    let error = transport
        .request(
            &transport.identity(),
            "responses",
            Some(json!({})),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "connection_changed");
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

fn delayed_body() -> (
    String,
    std::sync::mpsc::Sender<()>,
    std::thread::JoinHandle<()>,
) {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (release, wait) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut seen = Vec::new();
        let mut buffer = [0_u8; 8192];
        while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
            let count = socket.read(&mut buffer).unwrap();
            if count == 0 {
                break;
            }
            seen.extend_from_slice(&buffer[..count]);
        }
        let first = "data: {\"choices\":[{\"delta\":{\"content\":\"before\"}}]}\n\n";
        let last = "data: {\"choices\":[{\"delta\":{\"content\":\"after\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        write!(socket,"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{first}",first.len()+last.len()).unwrap();
        socket.flush().unwrap();
        let _ = wait.recv_timeout(std::time::Duration::from_secs(5));
        let _ = socket.write_all(last.as_bytes());
    });
    (url, release, server)
}

#[tokio::test]
async fn delayed_body_from_cross_handle_rotation_cannot_publish_stale_result() {
    let (url, release, server) = delayed_body();
    let store = TestStore::new();
    let id = register_provider_connection(url.clone(), Some("one".into()), Some(store.options()))
        .await
        .unwrap();
    let transport = BindingTransport::connect("p", "chat-completions", &id, open_fence())
        .await
        .unwrap();
    let provider = ChatCompletionsProvider::new(transport).unwrap();
    let mut events = provider.stream_chat(
        ChatRequest {
            model: "m".into(),
            ..Default::default()
        },
        CancellationToken::new(),
    );
    loop {
        match tokio::time::timeout(std::time::Duration::from_secs(5), events.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
        {
            ProviderEvent::Delta(text) if text == "before" => break,
            _ => {}
        }
    }
    agentkit_store::register_scoped_provider_connection(
        &store.path,
        "main",
        Some("two"),
        &url,
        "account",
        1000,
    )
    .unwrap();
    release.send(()).unwrap();
    let error = tokio::time::timeout(std::time::Duration::from_secs(5), events.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert_eq!(error.code, "connection_changed");
    assert!(events.next().await.is_none());
    server.join().unwrap();
}

#[tokio::test]
async fn failed_restore_rolls_back_durable_generation() {
    let store = TestStore::new();
    let id = register_provider_connection(
        "http://127.0.0.1:1".into(),
        Some("one".into()),
        Some(store.options()),
    )
    .await
    .unwrap();
    let mut options: Value = serde_json::from_str(&store.options()).unwrap();
    options["restoreContinuity"] =
        json!({"expectedGeneration":9,"previousGeneration":1,"confirmUnchangedScope":true});
    assert!(
        register_provider_connection(
            "http://127.0.0.1:1".into(),
            Some("two".into()),
            Some(options.to_string())
        )
        .await
        .is_err()
    );
    agentkit_store::check_provider_connection(&store.path, &id, 1).unwrap();
}
