use super::*;

// ----- review §2.3: scoped exact resume -------------------------------

#[tokio::test]
async fn completed_turn_commits_scoped_continuation_binding() {
    let h = harness();
    h.shared
        .queue(&[r#"{"e":"init","s":"vendor-1","m":"sonnet-x"}"#]);
    h.shared.set_outcome(Shared::completed("vendor-1", "hello"));
    h.submit(
        "t1",
        &scoped_payload(
            &native_target(NativeProfile::PlainChat, false),
            &fresh_input(),
            None,
            Some(&scope("convo-a", "parent-1", 1)),
        ),
    )
    .await;
    let claimed = h.claim("t1").await;
    h.executor()
        .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
        .await
        .expect("turn completes");
    let binding = h
        .store
        .active_continuation(
            "convo-a",
            "parent-1:answer",
            2,
            BACKEND,
            "/usr/bin/claude",
            "cfg-digest",
            None,
        )
        .await
        .unwrap()
        .expect("completion commits the durable binding at the COMPLETED position");
    assert_eq!(binding.vendor_session_id, "vendor-1");
    assert_eq!(binding.owning_task_id, "t1");
    assert!(
        h.store
            .active_continuation(
                "convo-a",
                "parent-1",
                1,
                BACKEND,
                "/usr/bin/claude",
                "cfg-digest",
                None
            )
            .await
            .unwrap()
            .is_none(),
        "the INPUT position must carry no binding — regenerating from it must miss"
    );
    assert!(
        h.event_types("t1")
            .await
            .iter()
            .any(|t| t == "native.continuation.bound"),
        "the binding event must be visible"
    );
}

#[tokio::test]
async fn exact_resume_rejects_foreign_scope_binding() {
    let h = harness();
    // Establish vendor-1 under conversation A.
    h.shared
        .queue(&[r#"{"e":"init","s":"vendor-1","m":"sonnet-x"}"#]);
    h.shared.set_outcome(Shared::completed("vendor-1", "hello"));
    h.submit(
        "t1",
        &scoped_payload(
            &native_target(NativeProfile::PlainChat, false),
            &fresh_input(),
            None,
            Some(&scope("convo-a", "parent-1", 1)),
        ),
    )
    .await;
    let claimed = h.claim("t1").await;
    h.executor()
        .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
        .await
        .unwrap();
    assert_eq!(h.shared.spawn_count.load(Ordering::SeqCst), 1);

    // Another conversation may NOT exact-resume the id even knowing it:
    // a vendor session is not global authority (review §2.3).
    let input = turn_input(TurnResume::Exact {
        vendor_session_id: "vendor-1".into(),
    });
    h.submit(
        "t2",
        &scoped_payload(
            &native_target(NativeProfile::PlainChat, false),
            &input,
            None,
            Some(&scope("convo-b", "parent-9", 7)),
        ),
    )
    .await;
    let claimed = h.claim("t2").await;
    let err = h
        .executor()
        .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
        .await
        .unwrap_err();
    let (code, retryable) = Harness::failure(err);
    assert_eq!(code.as_deref(), Some("continuation_mismatch"));
    assert_eq!(retryable, Some(false));
    assert_eq!(
        h.shared.spawn_count.load(Ordering::SeqCst),
        1,
        "a refused resume must never reach the vendor"
    );
}

#[tokio::test]
async fn exact_resume_follows_selected_history_position() {
    let h = harness();
    h.shared
        .queue(&[r#"{"e":"init","s":"vendor-1","m":"sonnet-x"}"#]);
    h.shared.set_outcome(Shared::completed("vendor-1", "hello"));
    let target = native_target(NativeProfile::PlainChat, false);
    h.submit(
        "t1",
        &scoped_payload(
            &target,
            &fresh_input(),
            None,
            Some(&scope("convo-a", "parent-1", 1)),
        ),
    )
    .await;
    let claimed = h.claim("t1").await;
    h.executor()
        .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
        .await
        .unwrap();

    // The follow-up is submitted from the position the first answer
    // completed at — the binding committed there is the attestation.
    h.shared
        .queue(&[r#"{"e":"init","s":"vendor-1","m":"sonnet-x"}"#]);
    h.shared.set_outcome(Shared::completed("vendor-1", "again"));
    let input = turn_input(TurnResume::Exact {
        vendor_session_id: "vendor-1".into(),
    });
    h.submit(
        "t2",
        &scoped_payload(
            &target,
            &input,
            None,
            Some(&scope("convo-a", "parent-1:answer", 2)),
        ),
    )
    .await;
    let claimed = h.claim("t2").await;
    h.executor()
        .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
        .await
        .expect("matching scope resumes exactly");
    assert_eq!(h.shared.spawn_count.load(Ordering::SeqCst), 2);
    assert!(
        h.event_types("t2")
            .await
            .iter()
            .any(|t| t == "native.completed")
    );

    // A rewind to an older selection must NOT inherit the future the
    // conversation already moved past: no active binding at the older
    // position means no exact continuation — fail closed.
    h.submit(
        "t3",
        &scoped_payload(
            &target,
            &input,
            None,
            Some(&scope("convo-a", "parent-0", 0)),
        ),
    )
    .await;
    let claimed = h.claim("t3").await;
    let err = h
        .executor()
        .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
        .await
        .unwrap_err();
    let (code, _) = Harness::failure(err);
    assert_eq!(code.as_deref(), Some("continuation_mismatch"));
    assert_eq!(
        h.shared.spawn_count.load(Ordering::SeqCst),
        2,
        "a rewound selection must not resurrect the later session"
    );
}

#[tokio::test]
async fn changed_configuration_refuses_exact_resume() {
    let h = harness();
    h.shared
        .queue(&[r#"{"e":"init","s":"vendor-1","m":"sonnet-x"}"#]);
    h.shared.set_outcome(Shared::completed("vendor-1", "hello"));
    let target = native_target(NativeProfile::PlainChat, false);
    h.submit(
        "t1",
        &scoped_payload(
            &target,
            &fresh_input(),
            None,
            Some(&scope("convo-a", "parent-1", 1)),
        ),
    )
    .await;
    let claimed = h.claim("t1").await;
    h.executor()
        .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
        .await
        .unwrap();

    // The binding EXISTS at the position the first answer completed
    // at; only the changed config identity differs. The store's exact
    // lookup must filter it out on config identity, and a changed
    // prompt revision must break the match just as decisively.
    let mut reconfigured = target.clone();
    reconfigured.config_identity = "cfg-changed".into();
    reconfigured.system_prompt_revision = Some("prompt-v2".into());
    let input = turn_input(TurnResume::Exact {
        vendor_session_id: "vendor-1".into(),
    });
    h.submit(
        "t2",
        &scoped_payload(
            &reconfigured,
            &input,
            None,
            Some(&ContinuationScope {
                config_identity: "cfg-changed".into(),
                system_prompt_revision: Some("prompt-v2".into()),
                ..scope("convo-a", "parent-1:answer", 2)
            }),
        ),
    )
    .await;
    let claimed = h.claim("t2").await;
    let err = h
        .executor()
        .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
        .await
        .unwrap_err();
    let (code, _) = Harness::failure(err);
    assert_eq!(code.as_deref(), Some("continuation_mismatch"));
}

// ----- review round-4 F3: claim-scoped continuation ownership -----

/// Publish one ACTIVE binding at the INPUT position
/// (`conversation`, `parent`, `position`) of a follow-up turn: a
/// fresh scoped turn submitted from just before it completes and
/// commits there, then settles.
async fn publish_input_position(
    h: &Harness,
    conversation: &str,
    parent: &str,
    position: i64,
    session: &str,
) -> String {
    let source_parent = format!("{parent}:before");
    let seeding = ContinuationScope {
        commit_parent: parent.into(),
        commit_history_position: position,
        ..scope(conversation, &source_parent, position - 1)
    };
    h.shared
        .queue(&[format!(r#"{{"e":"init","s":"{session}","m":"sonnet-x"}}"#).as_str()]);
    h.shared.set_outcome(Shared::completed(session, "seeded"));
    h.submit(
        "t-seed",
        &scoped_payload(
            &native_target(NativeProfile::PlainChat, false),
            &fresh_input(),
            None,
            Some(&seeding),
        ),
    )
    .await;
    let claimed = h.claim("t-seed").await;
    h.executor()
        .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
        .await
        .expect("seeding turn completes");
    h.store
        .settle(agentkit_store::SettleTask {
            fence: claimed.lease.fence(),
            now: h.runtime.now_ms(),
            status: agentkit_store::TaskStatus::Completed,
            error: None,
            events: vec![],
            messages: vec![],
        })
        .await
        .unwrap();
    claimed.lease.attempt_id
}

#[tokio::test]
async fn stale_selection_during_preflight_never_spawns_or_claims_replacement() {
    let h = harness();
    publish_input_position(&h, "stale-selection", "msg-1", 1, "vendor-x").await;
    h.shared.preflight_entered.store(false, Ordering::SeqCst);
    h.shared.preflight_hang.store(true, Ordering::SeqCst);
    h.submit(
        "stale",
        &scoped_payload(
            &native_target(NativeProfile::PlainChat, false),
            &turn_input(TurnResume::Exact {
                vendor_session_id: "vendor-x".into(),
            }),
            None,
            Some(&scope("stale-selection", "msg-1", 1)),
        ),
    )
    .await;
    let claimed = h.claim("stale").await;
    let executor = h.executor();
    let context = h.context(&claimed, CancellationToken::new(), CancellationToken::new());
    let handle = tokio::spawn(async move { executor.execute(context).await });
    tokio::time::timeout(Duration::from_secs(2), async {
        while !h.shared.preflight_entered.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("selection reached preflight");
    h.submit("replacement", &json!({})).await;
    let replacement = h.claim("replacement").await;
    let y = h
        .store
        .commit_continuation(CommitContinuation {
            conversation: "stale-selection".into(),
            parent: "msg-1".into(),
            history_position: 1,
            backend: BACKEND.into(),
            executable: "/usr/bin/claude".into(),
            config_identity: "cfg-digest".into(),
            system_prompt_revision: None,
            vendor_session_id: "vendor-y".into(),
            owning_attempt_id: replacement.lease.attempt_id.clone(),
            owning_task_id: "replacement".into(),
            committed_at: h.runtime.now_ms(),
        })
        .await
        .unwrap();
    h.shared.preflight_hang.store(false, Ordering::SeqCst);
    let (code, _) = Harness::failure(handle.await.unwrap().unwrap_err());
    assert_eq!(code.as_deref(), Some("continuation_mismatch"));
    assert_eq!(
        h.shared.spawn_count.load(Ordering::SeqCst),
        1,
        "only seeding turn spawned"
    );
    let current = h
        .store
        .continuation_for_attempt(&replacement.lease.attempt_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.binding_id, y.binding_id);
    assert_eq!(current.state, "active", "replacement remains unclaimed");
}

#[tokio::test]
async fn abort_during_pending_delivery_keeps_continuation_uncertain() {
    let h = harness();
    let seeding =
        publish_input_position(&h, "pending-delivery", "msg-1", 1, "vendor-pending").await;
    h.shared.delivered.lock().unwrap().clear();
    h.shared.delivery_hang.store(true, Ordering::SeqCst);
    h.submit(
        "pending",
        &scoped_payload(
            &native_target(NativeProfile::PlainChat, false),
            &turn_input(TurnResume::Exact {
                vendor_session_id: "vendor-pending".into(),
            }),
            None,
            Some(&scope("pending-delivery", "msg-1", 1)),
        ),
    )
    .await;
    let claimed = h.claim("pending").await;
    let executor = h.executor();
    let context = h.context(&claimed, CancellationToken::new(), CancellationToken::new());
    let handle = tokio::spawn(async move { executor.execute(context).await });
    tokio::time::timeout(Duration::from_secs(2), async {
        while h.shared.delivered.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("delivery entered");
    handle.abort();
    assert!(handle.await.unwrap_err().is_cancelled());
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let row = h
                .store
                .continuation_for_attempt(&seeding)
                .await
                .unwrap()
                .unwrap();
            if row.state == "uncertain" {
                break;
            }
            assert_ne!(
                row.state, "active",
                "pending delivery must never release consumed input"
            );
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("aborted attempt settled uncertain");
}

#[tokio::test]
async fn interrupted_after_input_attempt_cannot_exact_resume_preclaim_position() {
    // Review round-4 F3-A reproduced: complete a turn recording its
    // vendor session, resume that session for a follow-up, deliver
    // the prompt, receive PARTIAL output, then interrupt. The input
    // position the interrupted attempt claimed must never present as
    // available for an exact resume again — the vendor session
    // already consumed the prompt and part of the answer.
    let h = harness();
    let seeding_attempt = publish_input_position(&h, "convo-f3a", "msg-1", 1, "vendor-f3a").await;
    assert!(
        h.store
            .active_continuation(
                "convo-f3a",
                "msg-1",
                1,
                BACKEND,
                "/usr/bin/claude",
                "cfg-digest",
                None
            )
            .await
            .unwrap()
            .is_some(),
        "the input position is resumable before the claim"
    );

    let input = turn_input(TurnResume::Exact {
        vendor_session_id: "vendor-f3a".into(),
    });
    h.submit(
        "t1",
        &scoped_payload(
            &native_target(NativeProfile::PlainChat, false),
            &input,
            None,
            Some(&scope("convo-f3a", "msg-1", 1)),
        ),
    )
    .await;
    let claimed = h.claim("t1").await;
    // Partial output, no root result, then a durable user stop.
    h.shared.queue(&[r#"{"e":"delta","t":"half"}"#]);
    h.shared.hang.store(true, Ordering::SeqCst);
    h.shared.set_outcome(NativeTerminalOutcome::Interrupted {
        partial_text: Some("half".into()),
    });
    let interruption = CancellationToken::new();
    interruption.cancel();
    let (code, _) = Harness::failure(
        h.executor()
            .execute(h.context(&claimed, CancellationToken::new(), interruption))
            .await
            .unwrap_err(),
    );
    assert_eq!(code.as_deref(), Some("native_interrupted"));
    assert!(
        !h.shared.delivered.lock().unwrap().is_empty(),
        "the input WAS delivered"
    );

    // The claimed position is now `uncertain`: never available, and
    // the committing attempt's audit row says so honestly.
    assert!(
        h.store
            .active_continuation(
                "convo-f3a",
                "msg-1",
                1,
                BACKEND,
                "/usr/bin/claude",
                "cfg-digest",
                None
            )
            .await
            .unwrap()
            .is_none(),
        "an interrupted attempt's pre-claim position is not available"
    );
    assert!(
        h.store
            .resumable_continuation(
                "convo-f3a",
                "msg-1",
                1,
                BACKEND,
                "/usr/bin/claude",
                "cfg-digest",
                None,
                &claimed.lease.attempt_id
            )
            .await
            .unwrap()
            .is_none(),
        "not even the claimant may exact-resume it"
    );
    let row = h
        .store
        .continuation_for_attempt(&seeding_attempt)
        .await
        .unwrap()
        .expect("row kept for audit");
    assert_eq!(
        row.state, "uncertain",
        "interruption after input is uncertain"
    );

    // A follow-up asking for the exact resume is refused from the
    // ledger — no second spawn from the consumed session.
    h.submit(
        "t2",
        &scoped_payload(
            &native_target(NativeProfile::PlainChat, false),
            &input,
            None,
            Some(&scope("convo-f3a", "msg-1", 1)),
        ),
    )
    .await;
    let claimed2 = h.claim("t2").await;
    let (code, _) = Harness::failure(
        h.executor()
            .execute(h.context(
                &claimed2,
                CancellationToken::new(),
                CancellationToken::new(),
            ))
            .await
            .unwrap_err(),
    );
    assert_eq!(code.as_deref(), Some("continuation_mismatch"));
    assert_eq!(
        h.shared.spawn_count.load(Ordering::SeqCst),
        2,
        "only the seeding turn and the interrupted attempt ever spawned"
    );
}

#[tokio::test]
async fn retired_continuations_refuse_the_same_task_resume() {
    // Review round-4 F3-B reproduced (OneMind Resume, engine.resume
    // on the SAME task): the task's own observed session proves
    // identity only. Once the conversation's continuations are
    // retired, the same-task resume path must NOT respawn from the
    // stale vendor session — the ledger is the authority.
    let h = harness();
    publish_input_position(&h, "convo-f3b", "msg-1", 1, "vendor-f3b").await;
    let input = turn_input(TurnResume::Exact {
        vendor_session_id: "vendor-f3b".into(),
    });
    h.submit(
        "t1",
        &scoped_payload(
            &native_target(NativeProfile::PlainChat, false),
            &input,
            None,
            Some(&scope("convo-f3b", "msg-1", 1)),
        ),
    )
    .await;
    let attempt1 = h.claim("t1").await;
    // The first attempt observes the identity, delivers the prompt,
    // and ends inconclusively (uncertain — no root result, input
    // accepted: the discharge marks the claim uncertain).
    h.shared.queue(&[
        r#"{"e":"init","s":"vendor-f3b","m":"sonnet-x"}"#,
        r#"{"e":"delta","t":"partial"}"#,
    ]);
    h.shared.set_outcome(NativeTerminalOutcome::Uncertain {
        class: FailureClass::ExitWithoutResult,
        detail: "review fixture: exited without a root result".into(),
        partial_text: None,
    });
    h.executor()
        .execute(h.context(
            &attempt1,
            CancellationToken::new(),
            CancellationToken::new(),
        ))
        .await
        .expect_err("no scripted outcome is never a success");
    assert!(
        h.store
            .current_native_binding("t1")
            .await
            .unwrap()
            .is_some(),
        "the task-local observation persists for the retry"
    );

    // The runner retries the SAME task (attempt 2), then the branch
    // retirement puts every continuation of the conversation out of
    // service (OneMind / user deletes the branch).
    h.store
        .retry_task(agentkit_store::RetryTask {
            fence: attempt1.lease.fence(),
            now: h.runtime.now_ms(),
            available_at: h.runtime.now_ms(),
            max_attempts: 5,
            error: "inconclusive vendor outcome".into(),
        })
        .await
        .unwrap();
    h.store.retire_continuations("convo-f3b").await.unwrap();

    let attempt2 = h.claim("t1").await;
    assert_ne!(attempt2.lease.attempt_id, attempt1.lease.attempt_id);
    let spawned_before = h.shared.spawn_count.load(Ordering::SeqCst);
    let (code, _) = Harness::failure(
        h.executor()
            .execute(h.context(
                &attempt2,
                CancellationToken::new(),
                CancellationToken::new(),
            ))
            .await
            .unwrap_err(),
    );
    assert_eq!(
        code.as_deref(),
        Some("continuation_mismatch"),
        "the observed shortcut must not outlive the ledger"
    );
    assert_eq!(
        h.shared.spawn_count.load(Ordering::SeqCst),
        spawned_before,
        "no spawn from the retired session"
    );
}

#[tokio::test]
async fn two_current_tasks_one_binding_exactly_one_claim_winner() {
    // Review round-4 F3-C reproduced: two current tasks for the same
    // conversation position must never both start from the same
    // exact binding. The atomic claim (store-level CAS) has exactly
    // one winner; the loser takes the fresh-session decision.
    let h = harness();
    publish_input_position(&h, "convo-f3c", "msg-1", 1, "vendor-f3c").await;
    let input = turn_input(TurnResume::Exact {
        vendor_session_id: "vendor-f3c".into(),
    });
    let scope = scope("convo-f3c", "msg-1", 1);
    h.submit(
        "t-a",
        &scoped_payload(
            &native_target(NativeProfile::PlainChat, false),
            &input,
            None,
            Some(&scope),
        ),
    )
    .await;
    h.submit(
        "t-b",
        &scoped_payload(
            &native_target(NativeProfile::PlainChat, false),
            &input,
            None,
            Some(&scope),
        ),
    )
    .await;
    let claim_a = h.claim("t-a").await;
    let claim_b = h.claim("t-b").await;

    // Task A takes the claim first (the concurrent winner).
    let won = h
        .store
        .claim_continuation(
            "convo-f3c",
            "msg-1",
            1,
            BACKEND,
            "/usr/bin/claude",
            "cfg-digest",
            None,
            "t-a",
            &claim_a.lease.attempt_id,
            h.runtime.now_ms(),
        )
        .await
        .unwrap()
        .expect("the first claim wins");
    assert_eq!(won.state, "owned");

    // Task B's executor must not consume it: refused from the ledger,
    // zero OS contact.
    let (code, _) = Harness::failure(
        h.executor()
            .execute(h.context(&claim_b, CancellationToken::new(), CancellationToken::new()))
            .await
            .unwrap_err(),
    );
    assert_eq!(code.as_deref(), Some("continuation_mismatch"));
    assert_eq!(
        h.shared.spawn_count.load(Ordering::SeqCst),
        1,
        "the loser never spawns (the only spawn so far is the seeding turn)"
    );

    // The winner runs its turn normally (claim re-entry is
    // idempotent) and completes.
    h.shared
        .queue(&[r#"{"e":"init","s":"vendor-f3c","m":"sonnet-x"}"#]);
    h.shared
        .set_outcome(Shared::completed("vendor-f3c", "continued"));
    h.executor()
        .execute(h.context(&claim_a, CancellationToken::new(), CancellationToken::new()))
        .await
        .expect("the claim winner resumes normally");
    assert_eq!(
        h.shared.spawn_count.load(Ordering::SeqCst),
        2,
        "seed + winner"
    );
    assert!(
        h.store
            .active_continuation(
                "convo-f3c",
                "msg-1:answer",
                2,
                BACKEND,
                "/usr/bin/claude",
                "cfg-digest",
                None
            )
            .await
            .unwrap()
            .is_some(),
        "completion publishes the new active position"
    );
}

#[tokio::test]
async fn contradictory_continuation_identity_is_refused_before_spawn() {
    // Review round-4 F8-b at the executor boundary: a frozen scope
    // whose configuration identity contradicts the frozen target is
    // refused at claim, before any OS contact.
    let h = harness();
    let input = turn_input(TurnResume::Exact {
        vendor_session_id: "vendor-x".into(),
    });
    let drifted = ContinuationScope {
        config_identity: "cfg-other".into(),
        ..scope("convo-f8b", "msg-1", 1)
    };
    h.submit(
        "t1",
        &scoped_payload(
            &native_target(NativeProfile::PlainChat, false),
            &input,
            None,
            Some(&drifted),
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
    assert_eq!(code.as_deref(), Some("continuation_scope_mismatch"));
    assert_eq!(h.shared.spawn_count.load(Ordering::SeqCst), 0);
}
