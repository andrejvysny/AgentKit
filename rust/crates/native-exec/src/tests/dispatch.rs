use super::*;

struct CountingExecutor {
    hits: Arc<AtomicUsize>,
}

#[async_trait]
impl TaskExecutor<SqliteStore> for CountingExecutor {
    async fn execute(&self, _context: ExecutionContext) -> HostResult<()> {
        self.hits.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test]
async fn dispatch_executor_routes_by_frozen_target() {
    let h = harness();
    let native_hits = Arc::new(AtomicUsize::new(0));
    let fallback_hits = Arc::new(AtomicUsize::new(0));
    let dispatcher = NativeDispatchExecutor {
        native: Arc::new(CountingExecutor {
            hits: native_hits.clone(),
        }),
        fallback: Arc::new(CountingExecutor {
            hits: fallback_hits.clone(),
        }),
    };
    let cases: Vec<(Value, usize, usize)> = vec![
        (
            payload(
                &native_target(NativeProfile::PlainChat, false),
                &fresh_input(),
                None,
            ),
            1,
            0,
        ),
        (json!({"prompt": "legacy chat task"}), 0, 1),
        (
            json!({
                EXECUTION_TARGET_PAYLOAD_KEY: {
                    "modelLoop": {
                        "schemaVersion": 1,
                        "providerId": "p",
                        "model": "m"
                    }
                }
            }),
            0,
            1,
        ),
    ];
    for (index, (value, native_expected, fallback_expected)) in cases.into_iter().enumerate() {
        let task_id = format!("d{index}");
        h.submit(&task_id, &value).await;
        let claimed = h.claim(&task_id).await;
        let before_native = native_hits.load(Ordering::SeqCst);
        let before_fallback = fallback_hits.load(Ordering::SeqCst);
        dispatcher
            .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
            .await
            .unwrap();
        assert_eq!(
            native_hits.load(Ordering::SeqCst) - before_native,
            native_expected,
            "native route count for case {index}"
        );
        assert_eq!(
            fallback_hits.load(Ordering::SeqCst) - before_fallback,
            fallback_expected,
            "fallback route count for case {index}"
        );
    }

    // Reviewer F5: a frozen-but-malformed native target is terminal at
    // the dispatcher - it must never be silently executed (and billed)
    // by the model-loop fallback.
    let malformed = json!({
        EXECUTION_TARGET_PAYLOAD_KEY: {"native": {"schemaVersion": 1}},
    });
    h.submit("d-bad", &malformed).await;
    let claimed = h.claim("d-bad").await;
    let err = dispatcher
        .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
        .await
        .unwrap_err();
    let (code, retryable) = Harness::failure(err);
    assert_eq!(code.as_deref(), Some("execution_target_invalid"));
    assert_eq!(retryable, Some(false));
    assert_eq!(native_hits.load(Ordering::SeqCst), 1);
    assert_eq!(
        fallback_hits.load(Ordering::SeqCst),
        2,
        "the malformed native target never reached the fallback"
    );
}

struct CapExecutor {
    verification: bool,
    snapshots: bool,
}

#[async_trait]
impl TaskExecutor<SqliteStore> for CapExecutor {
    fn handles_verification(&self) -> bool {
        self.verification
    }
    fn resolves_provider_snapshots(&self) -> bool {
        self.snapshots
    }
    async fn execute(&self, _context: ExecutionContext) -> HostResult<()> {
        Ok(())
    }
}

#[tokio::test]
async fn dispatch_executor_preserves_declared_capabilities() {
    // Review §3.4: wrapping must not reclassify a turn. Each route's
    // declared verification / snapshot capabilities survive the wrapper.
    let dispatcher = NativeDispatchExecutor {
        native: Arc::new(CapExecutor {
            verification: true,
            snapshots: false,
        }),
        fallback: Arc::new(CapExecutor {
            verification: false,
            snapshots: true,
        }),
    };
    let native_payload = payload(
        &native_target(NativeProfile::PlainChat, false),
        &fresh_input(),
        None,
    );
    let model_payload = json!({
        EXECUTION_TARGET_PAYLOAD_KEY: {
            "modelLoop": { "schemaVersion": 1, "providerId": "p", "model": "m" }
        }
    });
    let legacy = json!({"prompt": "legacy chat task"});

    assert!(
        dispatcher.handles_verification_for(&native_payload),
        "the native route keeps its self-attested verification"
    );
    assert!(
        !dispatcher.handles_verification_for(&model_payload),
        "the model-loop route keeps its hook-verified behaviour"
    );
    assert!(!dispatcher.handles_verification_for(&legacy));
    assert!(
        !dispatcher.resolves_provider_snapshots_for(&native_payload),
        "a native turn resolves no provider snapshot"
    );
    assert!(
        dispatcher.resolves_provider_snapshots_for(&model_payload),
        "the model-loop route keeps dynamic snapshot resolution"
    );
    assert!(dispatcher.resolves_provider_snapshots_for(&legacy));

    // The kind-wide host view follows the model-loop lane — that is
    // the behaviour outside callers (HTTP admission) condition on.
    assert!(!dispatcher.handles_verification());
    assert!(dispatcher.resolves_provider_snapshots());
}
