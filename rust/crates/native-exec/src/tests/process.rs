use super::*;

// ----- review §2.5: lifetime under cancellation -----------------------

#[tokio::test]
async fn aborted_execution_revokes_and_shuts_down_the_broker() {
    let h = harness();
    // A child that never exits: the future stays in the stream loop
    // after the broker has started, exactly like a hung vendor CLI.
    h.shared.hang.store(true, Ordering::SeqCst);
    h.submit(
        "t1",
        &payload(
            &native_target(NativeProfile::OwnedMcpChat, false),
            &fresh_input(),
            Some(&broker_spec()),
        ),
    )
    .await;
    let claimed = h.claim("t1").await;
    let executor = h.executor();
    let context = h.context(&claimed, CancellationToken::new(), CancellationToken::new());
    let handle = tokio::spawn(async move { executor.execute(context).await });
    let broker = loop {
        if let Some(broker) = h.factory.started.lock().unwrap().last().cloned() {
            break broker;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    };
    // Whole-future cancellation — no explicit cleanup ran anywhere.
    handle.abort();
    let _ = handle.await;

    // Every ingress must refuse on its own (owner Drop revoked the
    // authority — no explicit shutdown ran anywhere). The cleanup is
    // detached, so give it a bounded window before judging.
    let scope = SessionScope {
        principal: json!({"attempt": "aborted"}),
        chat_id: None,
        actor_id: "vendor-1".into(),
        request_id: None,
        cancellation: CancellationToken::new(),
    };
    let mut refusal = None;
    for _ in 0..400 {
        let tools = broker.backend().list_tools(&scope).await.err();
        // A revoked broker refuses a call as an isError result (the
        // MCP call channel reports refusals in-band, not as Err).
        let call = match broker
            .backend()
            .call_tool("note_search", Default::default(), &scope)
            .await
        {
            Ok(result) if result.is_error == Some(true) => Some(format!("{result:?}")),
            Ok(_result) => {
                tokio::time::sleep(Duration::from_millis(5)).await;
                continue;
            }
            Err(error) => Some(format!("{error:?}")),
        };
        if let Some(tools_err) = tools {
            let quarantined = h
                .shared
                .interrupts
                .lock()
                .unwrap()
                .iter()
                .any(|policy| matches!(policy, InterruptPolicy::Quarantine));
            if quarantined {
                refusal = Some((format!("{tools_err:?}"), call.unwrap_or_default()));
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let refusal = match refusal {
        Some(refusal) => refusal,
        None => {
            let tools_dbg = broker.backend().list_tools(&scope).await.is_err();
            let call_dbg = broker
                .backend()
                .call_tool("note_search", Default::default(), &scope)
                .await
                .is_err();
            let interrupts = format!("{:?}", *h.shared.interrupts.lock().unwrap());
            let live = h.shared.child_live.load(Ordering::SeqCst);
            panic!(
                "broker serving or child un-quarantined: tools_err={tools_dbg} call_err={call_dbg} interrupts={interrupts} child_live={live}"
            )
        }
    };
    let (tools_text, call_text) = refusal;
    let text = format!("{tools_text} {call_text}");
    assert!(
        text.contains("revoked") || text.contains("not_live") || text.contains("denied"),
        "expected an explicit revocation refusal, got {text}"
    );
}

/// AGENTKIT-110 lifecycle hardening: the attempt destructor is scoped
/// to the attempt's OWN process generation. Dropping attempt A must
/// never quarantine-interrupt a child belonging to a concurrent
/// attempt B on the shared runtime supervisor; dropping the owner that
/// DID attach a child must terminate exactly that child.
#[tokio::test]
async fn dropping_an_attempt_owner_never_interrupts_a_sibling_child() {
    let shared = Shared::default();
    let sup: Arc<dyn ProcessSupervisor> = Arc::new(FakeSupervisor {
        shared: shared.clone(),
    });
    let plan = LaunchPlan {
        argv: vec!["claude".into()],
        env: vec![],
        cwd: "/tmp".into(),
        per_run_dir: "/tmp/run".into(),
        permission_policy: PermissionPromptPolicy::None,
        max_vendor_turns: None,
        native_session_id: None,
        fingerprint: "fp-scope".into(),
    };
    let mut child = sup
        .spawn(&plan, &ResourceLimits::default(), &CancellationToken::new())
        .await
        .expect("spawn sibling child");
    assert!(shared.child_live.load(Ordering::SeqCst));

    // Attempt A owns no process: dropping it must leave B untouched.
    let owner_a = AttemptOwner::new(
        SqliteStore::memory(64).unwrap(),
        "attempt-a",
        sup.clone(),
        None,
        None,
    );
    drop(owner_a);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        shared.interrupts.lock().unwrap().is_empty(),
        "dropping an owner with no attached process must not interrupt the sibling"
    );
    assert!(
        shared.child_live.load(Ordering::SeqCst),
        "the sibling child must survive another attempt's drop"
    );

    // Attempt B owns the live child: its drop terminates exactly it.
    let owner_b = AttemptOwner::new(
        SqliteStore::memory(64).unwrap(),
        "attempt-b",
        sup.clone(),
        None,
        None,
    );
    owner_b.attach_child(child.pid(), child.generation());
    drop(owner_b);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while shared.child_live.load(Ordering::SeqCst) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the owner that attached the child must terminate its own process"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        &*shared.interrupts.lock().unwrap(),
        &[InterruptPolicy::Quarantine],
        "exactly one quarantine interrupt — the attached generation only"
    );
    let _ = child.wait().await;
}

#[tokio::test]
async fn post_spawn_persistence_failure_reaps_the_child() {
    let h = harness();
    h.shared
        .queue(&[r#"{"e":"init","s":"vendor-1","m":"sonnet-x"}"#]);
    h.shared.set_outcome(Shared::completed("vendor-1", "hello"));
    h.submit(
        "t1",
        &payload(
            &native_target(NativeProfile::PlainChat, false),
            &fresh_input(),
            None,
        ),
    )
    .await;
    let claimed = h.claim("t1").await;
    // Surrender the lease before execution starts: the first event the
    // executor appends AFTER spawning is rejected at the fence — the
    // review §2.5 `?`-leak shape.
    h.store
        .park_interrupted(claimed.lease.fence(), h.runtime.now_ms())
        .await
        .unwrap();
    let err = h
        .executor()
        .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
        .await
        .unwrap_err();
    assert!(
        matches!(err, HostError::Store(_)),
        "expected the fenced append failure to surface, got {err:?}"
    );
    assert_eq!(h.shared.spawn_count.load(Ordering::SeqCst), 1);
    assert!(
        h.shared
            .interrupts
            .lock()
            .unwrap()
            .iter()
            .any(|policy| matches!(policy, InterruptPolicy::Quarantine)),
        "a persistence failure after spawn must quarantine-interrupt and reap the child"
    );
    assert!(
        h.shared
            .interrupts
            .lock()
            .unwrap()
            .iter()
            .any(|policy| matches!(policy, InterruptPolicy::HostShutdown)
                || matches!(policy, InterruptPolicy::Quarantine))
            && !h
                .event_types("t1")
                .await
                .iter()
                .any(|t| t == "native.completed"),
        "no completion may be claimed for the failed attempt"
    );
}

#[tokio::test]
async fn preflight_refusal_never_spawns() {
    let h = harness();
    *h.shared.preflight_refusal.lock().unwrap() =
        Some(NativeRefusal::new("not_signed_in", "sign in first", true));
    h.submit(
        "t1",
        &payload(
            &native_target(NativeProfile::PlainChat, false),
            &fresh_input(),
            None,
        ),
    )
    .await;
    let claimed = h.claim("t1").await;
    let (code, retryable) = Harness::failure(
        h.executor()
            .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
            .await
            .unwrap_err(),
    );
    assert_eq!(code.as_deref(), Some("not_signed_in"));
    assert_eq!(retryable, Some(true), "pre-spawn refusal is replay-safe");
    assert_eq!(h.shared.spawn_count.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn host_cancellation_interrupts_child_and_reports_cancelled() {
    let h = harness();
    h.shared.hang.store(true, Ordering::SeqCst);
    h.shared
        .set_outcome(NativeTerminalOutcome::Interrupted { partial_text: None });
    h.submit(
        "t1",
        &payload(
            &native_target(NativeProfile::PlainChat, false),
            &fresh_input(),
            None,
        ),
    )
    .await;
    let claimed = h.claim("t1").await;
    let cancellation = CancellationToken::new();
    let context = h.context(&claimed, cancellation.clone(), CancellationToken::new());
    cancellation.cancel();
    let err = h.executor().execute(context).await.unwrap_err();
    assert!(matches!(err, HostError::Cancelled), "{err:?}");
    assert_eq!(
        *h.shared.interrupts.lock().unwrap(),
        [InterruptPolicy::HostShutdown]
    );
}

#[tokio::test]
async fn durable_interruption_is_terminal_not_cancelled() {
    let h = harness();
    h.shared.hang.store(true, Ordering::SeqCst);
    h.shared
        .set_outcome(NativeTerminalOutcome::Interrupted { partial_text: None });
    h.submit(
        "t1",
        &payload(
            &native_target(NativeProfile::PlainChat, false),
            &fresh_input(),
            None,
        ),
    )
    .await;
    let claimed = h.claim("t1").await;
    let interruption = CancellationToken::new();
    let context = h.context(&claimed, CancellationToken::new(), interruption.clone());
    interruption.cancel();
    let (code, retryable) = Harness::failure(h.executor().execute(context).await.unwrap_err());
    assert_eq!(code.as_deref(), Some("native_interrupted"));
    assert_eq!(
        retryable,
        Some(false),
        "a stopped turn is never auto-retried"
    );
    assert_eq!(
        *h.shared.interrupts.lock().unwrap(),
        [InterruptPolicy::UserStop]
    );
}

#[tokio::test]
async fn protocol_violation_quarantines_the_child() {
    let h = harness();
    h.shared.queue(&["VIOLATION junk"]);
    h.submit(
        "t1",
        &payload(
            &native_target(NativeProfile::PlainChat, false),
            &fresh_input(),
            None,
        ),
    )
    .await;
    let claimed = h.claim("t1").await;
    let (code, retryable) = Harness::failure(
        h.executor()
            .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
            .await
            .unwrap_err(),
    );
    assert_eq!(code.as_deref(), Some("native_protocol_violation"));
    assert_eq!(retryable, Some(false));
    assert_eq!(
        *h.shared.interrupts.lock().unwrap(),
        [InterruptPolicy::Quarantine]
    );
}

#[tokio::test]
async fn idle_timeout_stops_silent_child() {
    let h = harness();
    // One event, then silence: the idle bound must fire. Pop-first
    // guarantees the init frame is consumed before the hang engages.
    h.shared.queue(&[r#"{"e":"init","s":"vendor-1"}"#]);
    h.shared.hang.store(true, Ordering::SeqCst);
    h.submit(
        "t1",
        &payload(
            &native_target(NativeProfile::PlainChat, false),
            &fresh_input(),
            None,
        ),
    )
    .await;
    let claimed = h.claim("t1").await;
    let (code, retryable) = Harness::failure(
        h.executor()
            .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
            .await
            .unwrap_err(),
    );
    assert_eq!(code.as_deref(), Some("native_idle_timeout"));
    assert_eq!(
        retryable,
        Some(false),
        "an idle child may have acted; no blind retry"
    );
    assert!(!h.shared.interrupts.lock().unwrap().is_empty());
}

#[tokio::test]
async fn init_identity_drift_fails_closed() {
    let h = harness();
    *h.shared.preassigned.lock().unwrap() = Some("host-1".into());
    h.shared.queue(&[r#"{"e":"init","s":"vendor-2"}"#]);
    h.shared.set_outcome(Shared::completed("vendor-2", "late"));
    h.submit(
        "t1",
        &payload(
            &native_target(NativeProfile::PlainChat, false),
            &fresh_input(),
            None,
        ),
    )
    .await;
    let claimed = h.claim("t1").await;
    let (code, _) = Harness::failure(
        h.executor()
            .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
            .await
            .unwrap_err(),
    );
    assert_eq!(code.as_deref(), Some("session_identity_mismatch"));
    assert_eq!(
        *h.shared.interrupts.lock().unwrap(),
        [InterruptPolicy::Quarantine]
    );
}
