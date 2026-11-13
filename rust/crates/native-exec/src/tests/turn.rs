use super::*;

// ----- tests ------------------------------------------------------------

#[tokio::test]
async fn completed_turn_binds_lineage_and_emits() {
    let h = harness();
    h.shared.queue(&[
        r#"{"e":"init","s":"vendor-1","m":"sonnet-x"}"#,
        r#"{"e":"delta","t":"hel"}"#,
        r#"{"e":"delta","t":"lo"}"#,
    ]);
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
    let executor = h.executor();
    executor
        .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
        .await
        .expect("turn completes");
    let binding = h.store.current_native_binding("t1").await.unwrap().unwrap();
    assert_eq!(binding.vendor_session_id, "vendor-1");
    assert_eq!(binding.state, "observed");
    let types = h.event_types("t1").await;
    for expected in [
        "native.spawn",
        "native.event",
        "native.session.observed",
        "native.completed",
    ] {
        assert!(
            types.iter().any(|t| t == expected),
            "missing {expected} in {types:?}"
        );
    }
    assert_eq!(
        h.shared.delivered.lock().unwrap().as_slice(),
        [b"hello".to_vec()]
    );
    assert_eq!(h.shared.spawn_count.load(Ordering::SeqCst), 1);
}

/// Round-4 F5: an unconfirmed normal-completion process-group reconcile
/// must reach the durable attempt record as `cleanupUncertain: true`.
/// No interrupt ladder ran and no transport fault occurred, so the
/// supervisor's read-back is the ONLY possible source of the flag.
#[tokio::test]
async fn unconfirmed_descendant_cleanup_marks_completed_attempt_uncertain() {
    let h = harness();
    h.shared.queue(&[
        r#"{"e":"init","s":"vendor-1","m":"sonnet-x"}"#,
        r#"{"e":"delta","t":"hel"}"#,
        r#"{"e":"delta","t":"lo"}"#,
    ]);
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
    let executor = h.executor_with(Arc::new(UncertainDescendantSupervisor {
        inner: FakeSupervisor {
            shared: h.shared.clone(),
        },
    }));
    executor
        .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
        .await
        .expect("the turn itself completes normally");
    let completed = h
        .store
        .events("t1", None, 4096)
        .await
        .unwrap()
        .into_iter()
        .find(|record| {
            record.event["type"]
                .as_str()
                .is_some_and(|t| t.ends_with("native.completed"))
        })
        .expect("a completed attempt record");
    assert!(
        h.shared.interrupts.lock().unwrap().is_empty(),
        "no ladder ran: the supervisor report alone must set the flag"
    );
    assert_eq!(
        completed.event["data"]["cleanupUncertain"],
        json!(true),
        "unverified descendants must be visible uncertainty on a completed turn: {:?}",
        completed.event
    );
}

/// Round-4 F5 (pinned default): a supervisor that reports nothing — the
/// port default, what every pre-existing implementor gets — leaves a
/// completed turn's record clean, exactly as before the F5 wiring.
#[tokio::test]
async fn reported_nothing_keeps_completed_attempt_certain() {
    let h = harness();
    h.shared.queue(&[
        r#"{"e":"init","s":"vendor-1","m":"sonnet-x"}"#,
        r#"{"e":"delta","t":"hel"}"#,
        r#"{"e":"delta","t":"lo"}"#,
    ]);
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
    h.executor()
        .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
        .await
        .expect("the turn completes normally");
    let completed = h
        .store
        .events("t1", None, 4096)
        .await
        .unwrap()
        .into_iter()
        .find(|record| {
            record.event["type"]
                .as_str()
                .is_some_and(|t| t.ends_with("native.completed"))
        })
        .expect("a completed attempt record");
    assert_eq!(
        completed.event["data"]["cleanupUncertain"],
        json!(false),
        "no report and no fault must stay certain: {:?}",
        completed.event
    );
}

#[tokio::test]
async fn exit_without_result_is_uncertain_never_completed() {
    let h = harness();
    // No frames, no scripted outcome: EOF without a root result.
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
    let err = h
        .executor()
        .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
        .await
        .unwrap_err();
    let (code, retryable) = Harness::failure(err);
    assert_eq!(code.as_deref(), Some("native_exit_without_result"));
    assert_eq!(retryable, Some(true));
    assert!(
        h.event_types("t1")
            .await
            .iter()
            .any(|t| t == "native.uncertain")
    );
    assert!(
        h.store
            .current_native_binding("t1")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn model_loop_target_is_refused_by_native_executor() {
    let h = harness();
    let value = json!({
        EXECUTION_TARGET_PAYLOAD_KEY: {"modelLoop": {
            "schemaVersion": agentkit_native::EXECUTION_TARGET_SCHEMA_VERSION,
            "providerId": "p", "model": "m"
        }},
        TURN_INPUT_PAYLOAD_KEY: serde_json::to_value(fresh_input()).unwrap(),
    });
    h.submit("t1", &value).await;
    let claimed = h.claim("t1").await;
    let (code, retryable) = Harness::failure(
        h.executor()
            .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
            .await
            .unwrap_err(),
    );
    assert_eq!(code.as_deref(), Some("wrong_dispatch"));
    assert_eq!(retryable, Some(false));
    assert_eq!(h.shared.spawn_count.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn unknown_capability_is_transient_structural_is_terminal() {
    let h = harness();
    h.submit(
        "t1",
        &payload(
            &native_target(NativeProfile::PlainChat, true),
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
    assert_eq!(code.as_deref(), Some("capability_violation"));
    assert_eq!(
        retryable,
        Some(true),
        "unknown-only evidence must be transient"
    );
    assert_eq!(h.shared.spawn_count.load(Ordering::SeqCst), 0);

    // Structural: images enabled without capability.
    let mut target = native_target(NativeProfile::PlainChat, false);
    target.content.images = true;
    let h = harness();
    h.submit("t2", &payload(&target, &fresh_input(), None))
        .await;
    let claimed = h.claim("t2").await;
    let (code, retryable) = Harness::failure(
        h.executor()
            .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
            .await
            .unwrap_err(),
    );
    assert_eq!(code.as_deref(), Some("capability_violation"));
    assert_eq!(retryable, Some(false));
}

#[tokio::test]
async fn unscoped_exact_resume_is_refused() {
    // AGENTKIT-113: the host-global "observed somewhere" fallback is
    // retired. Without a frozen scope there is no conversation,
    // configuration or position to authorise the resume.
    let h = harness();
    let input = turn_input(TurnResume::Exact {
        vendor_session_id: "v-old".into(),
    });
    h.submit(
        "t1",
        &payload(
            &native_target(NativeProfile::PlainChat, false),
            &input,
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
    assert_eq!(code.as_deref(), Some("continuation_scope_missing"));
    assert_eq!(h.shared.spawn_count.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn exact_resume_with_observed_lineage_runs_and_restates_binding() {
    let h = harness();
    let input = turn_input(TurnResume::Exact {
        vendor_session_id: "v-old".into(),
    });
    h.submit(
        "t1",
        &payload(
            &native_target(NativeProfile::PlainChat, false),
            &input,
            None,
        ),
    )
    .await;
    // A previous attempt observed the lineage.
    h.store
        .bind_native_session(BindNativeSession {
            attempt_id: "att-0".into(),
            task_id: "t1".into(),
            backend: BACKEND.into(),
            vendor_session_id: "v-old".into(),
            parent_vendor_session_id: None,
            assigned_at: h.runtime.now_ms(),
        })
        .await
        .unwrap();
    h.store
        .observe_native_session("att-0", "v-old", h.runtime.now_ms())
        .await
        .unwrap();
    h.shared
        .set_outcome(Shared::completed("v-old", "continued"));
    let claimed = h.claim("t1").await;
    h.executor()
        .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
        .await
        .expect("exact resume runs");
    let binding = h
        .store
        .current_native_binding("t1")
        .await
        .unwrap()
        .expect("observed lineage present");
    assert_eq!(binding.vendor_session_id, "v-old");
    assert_eq!(h.shared.spawn_count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn informational_preflight_notes_never_block_the_turn() {
    // Reviewer P1-1 regression: the driver's happy-path preflight is
    // informational (binary observed, sign-in observed). The executor
    // must fail open on notes and only refuse on blocking issues.
    let h = harness();
    h.shared.preflight_notes.lock().unwrap().extend([
        "official binary observed (version 2.1.310)".to_string(),
        "subscription sign-in observed (scope: account)".to_string(),
    ]);
    h.shared
        .queue(&[r#"{"e":"init","s":"v-note","m":"sonnet-x"}"#]);
    h.shared.set_outcome(Shared::completed("v-note", "noted"));
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
    h.executor()
        .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
        .await
        .expect("informational notes never block the turn");
    assert_eq!(h.shared.spawn_count.load(Ordering::SeqCst), 1);
    assert_eq!(
        h.store
            .current_native_binding("t1")
            .await
            .unwrap()
            .unwrap()
            .vendor_session_id,
        "v-note"
    );
}

#[tokio::test]
async fn blocking_preflight_issue_refuses_without_spawning() {
    // The mirror case: a genuine blocking issue is still a terminal
    // refusal with zero OS contact.
    let h = harness();
    h.shared
        .preflight_blocking
        .lock()
        .unwrap()
        .push("no official Claude Code binary found".into());
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
    assert_eq!(code.as_deref(), Some("preflight_not_ready"));
    assert_eq!(
        retryable,
        Some(false),
        "a blocking issue is an operator fix, not an automatic requeue"
    );
    assert_eq!(h.shared.spawn_count.load(Ordering::SeqCst), 0);
    assert!(
        h.store
            .current_native_binding("t1")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn cross_task_exact_resume_uses_previous_tasks_lineage() {
    // Reviewer P1-2 / ONEMIND-341 regression: a follow-up turn asks to
    // resume the vendor session a *previous* task observed. The
    // lineage lives on the earlier task's committed binding, not this
    // task; refusing it (the pre-fix behavior) killed every second
    // turn. AGENTKIT-113: the resume is authorised by the scoped
    // binding — this turn is submitted from the position the previous
    // answer completed at.
    let h = harness();
    // Task one starts fresh and the vendor mints the session.
    h.shared
        .queue(&[r#"{"e":"init","s":"v-shared","m":"sonnet-x"}"#]);
    h.shared.set_outcome(Shared::completed("v-shared", "first"));
    h.submit(
        "t-first",
        &scoped_payload(
            &native_target(NativeProfile::PlainChat, false),
            &fresh_input(),
            None,
            Some(&scope("convo-a", "parent-0", 0)),
        ),
    )
    .await;
    let claimed = h.claim("t-first").await;
    h.executor()
        .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
        .await
        .expect("first turn completes");

    // Task two is a brand-new task record resuming that exact session,
    // submitted from the position task one's answer completed at.
    let input = turn_input(TurnResume::Exact {
        vendor_session_id: "v-shared".into(),
    });
    h.shared
        .set_outcome(Shared::completed("v-shared", "second"));
    h.submit(
        "t-second",
        &scoped_payload(
            &native_target(NativeProfile::PlainChat, false),
            &input,
            None,
            Some(&scope("convo-a", "parent-0:answer", 1)),
        ),
    )
    .await;
    let claimed = h.claim("t-second").await;
    h.executor()
        .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
        .await
        .expect("cross-task exact resume runs, not lineage_missing");
    assert_eq!(
        h.shared.spawn_count.load(Ordering::SeqCst),
        2,
        "the resumed turn actually spawned"
    );
    let binding = h
        .store
        .current_native_binding("t-second")
        .await
        .unwrap()
        .expect("restated lineage");
    assert_eq!(binding.vendor_session_id, "v-shared");
    assert_eq!(binding.state, "observed");
}

#[tokio::test]
async fn unobserved_assigned_session_is_never_resumable() {
    // Reviewer F3 regression: a fresh turn pre-assigns our own session
    // id before spawn; if the vendor dies before `init` that id is a
    // phantom. It must stay `assigned` (never `observed`) and an Exact
    // resume of it must be refused as lineage_missing.
    let h = harness();
    *h.shared.preassigned.lock().unwrap() = Some("s-phantom".into());
    // No frames, no outcome: EOF without init or result.
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
    assert_eq!(code.as_deref(), Some("native_exit_without_result"));
    assert!(
        h.store
            .current_native_binding("t1")
            .await
            .unwrap()
            .is_none(),
        "an unobserved id is never a current binding"
    );
    assert!(
        !h.store
            .is_observed_native_session("s-phantom")
            .await
            .unwrap(),
        "never-echoed ids must not become observed lineage"
    );

    // A later turn asking to resume the phantom is honestly refused.
    let input = turn_input(TurnResume::Exact {
        vendor_session_id: "s-phantom".into(),
    });
    h.submit(
        "t2",
        &payload(
            &native_target(NativeProfile::PlainChat, false),
            &input,
            None,
        ),
    )
    .await;
    let claimed = h.claim("t2").await;
    let (code, _) = Harness::failure(
        h.executor()
            .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
            .await
            .unwrap_err(),
    );
    assert_eq!(code.as_deref(), Some("continuation_scope_missing"));
    // The scoped form is refused too — the ledger holds no binding.
    h.submit(
        "t3",
        &scoped_payload(
            &native_target(NativeProfile::PlainChat, false),
            &input,
            None,
            Some(&scope("convo-x", "parent-9", 3)),
        ),
    )
    .await;
    let claimed = h.claim("t3").await;
    let (code, _) = Harness::failure(
        h.executor()
            .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
            .await
            .unwrap_err(),
    );
    assert_eq!(code.as_deref(), Some("continuation_mismatch"));
    assert_eq!(
        h.shared.spawn_count.load(Ordering::SeqCst),
        1,
        "the phantom resume never spawned"
    );
}
