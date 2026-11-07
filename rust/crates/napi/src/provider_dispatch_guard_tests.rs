use super::*;

#[derive(Debug)]
struct SelectedGuard(Arc<AtomicBool>);
#[async_trait]
impl agentkit_core::DispatchGuard for SelectedGuard {
    async fn check(&self) -> std::result::Result<(), CoreError> {
        if self.0.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err(CoreError::new(
                "selected_content_revoked",
                "Selected content grant was revoked",
            ))
        }
    }
    fn fence(&self) -> agentkit_core::DispatchFence {
        agentkit_core::DispatchFence {
            task_id: "run".into(),
            attempt_id: "attempt".into(),
            fencing_token: 1,
        }
    }
}
#[tokio::test]
async fn legacy_provider_checks_guard_after_the_secret_registry_wait() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    registered(
        "legacy-grant-guard",
        format!("http://{}", listener.local_addr().unwrap()),
        Some("synthetic-key".into()),
    )
    .await;
    let transport = BindingTransport::connect(
        "configured",
        "chat-completions",
        "legacy-grant-guard",
        open_fence(),
    )
    .await
    .unwrap();
    let alive = Arc::new(AtomicBool::new(true));
    let guard = Arc::new(SelectedGuard(alive.clone()));
    let registry = CONNECTIONS.lock().await;
    let pending = tokio::spawn(async move {
        transport
            .request_guarded(
                &transport.identity(),
                "chat/completions",
                Some(serde_json::json!({"model":"model"})),
                Some(guard),
                CancellationToken::new(),
            )
            .await
    });
    tokio::task::yield_now().await;
    alive.store(false, Ordering::SeqCst);
    drop(registry);
    let error = tokio::time::timeout(std::time::Duration::from_secs(1), pending)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert_eq!(error.code, "selected_content_revoked");
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}
