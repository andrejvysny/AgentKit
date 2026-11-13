use super::*;

/// Round-4 F6a contract: the stanza carries the consumer's frozen
/// authorization policy as an OPAQUE value (parse-and-preserve;
/// `deny_unknown_fields` still refuses anything unnamed).
#[test]
fn broker_stanza_carries_the_frozen_policy_opaquely() {
    let with_policy = json!({
        "serverName": "onemind",
        "tools": [{"name": "note_search", "inputSchema": {"type": "object"}}],
        "authorizationPolicy": {
            "autoApproveReadOnly": false,
            "approvalTimeoutMs": 300000,
            "maxCallsPerTurn": 8,
            "turnDeadlineMs": 300000,
            "installAllowlist": ["note_search", "note_read"]
        }
    });
    let spec: BrokerSpec = serde_json::from_value(with_policy.clone()).unwrap();
    assert_eq!(
        spec.authorization_policy.as_ref().unwrap(),
        with_policy.get("authorizationPolicy").unwrap(),
        "the policy travels verbatim — the host never reshapes it"
    );
    // Absent stays None (pre-F6a stanzas keep parsing).
    let without = json!({
        "serverName": "onemind",
        "tools": [{"name": "note_search", "inputSchema": {"type": "object"}}]
    });
    assert!(
        serde_json::from_value::<BrokerSpec>(without)
            .unwrap()
            .authorization_policy
            .is_none()
    );
    // The gate stays strict for keys nobody declared.
    let mut rogue = with_policy;
    rogue["authorizationPolicy"]["injected"] = json!(true);
    assert!(
        serde_json::from_value::<BrokerSpec>(rogue.clone()).is_ok(),
        "the carrier is opaque"
    );
    rogue["rogueKey"] = json!(1);
    assert!(
        serde_json::from_value::<BrokerSpec>(rogue).is_err(),
        "deny_unknown_fields still fences the stanza shape"
    );
}

#[tokio::test]
async fn owned_mcp_chat_wires_broker_then_revokes() {
    let h = harness();
    h.shared
        .set_outcome(Shared::completed("vendor-1", "tool answer"));
    h.shared
        .queue(&[r#"{"e":"init","s":"vendor-1","mcp":{"onemind":"connected"}}"#]);
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
    h.executor()
        .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
        .await
        .expect("governed turn completes");
    // Exactly one broker for this attempt, spec handed to the driver.
    assert_eq!(h.factory.contexts.lock().unwrap().len(), 1);
    let recorded = h.factory.contexts.lock().unwrap()[0].clone();
    let (task_id, attempt_id, server, tools) = &recorded;
    assert_eq!(task_id, "t1");
    assert_eq!(attempt_id, &claimed.lease.attempt_id);
    assert_eq!(server, "onemind");
    assert_eq!(*tools, 1);
    let seen = h.shared.seen_mcp.lock().unwrap().clone().unwrap();
    assert!(seen.is_some_and(|spec| spec.http_url.is_some() && spec.auth_token.is_some()));
    // Settlement revoked the endpoint immediately.
    let broker = h.factory.started.lock().unwrap().remove(0);
    let scope = agentkit_mcp::SessionScope {
        principal: json!({"attempt": claimed.lease.attempt_id}),
        chat_id: None,
        actor_id: "vendor-1".into(),
        request_id: None,
        cancellation: CancellationToken::new(),
    };
    assert!(
        broker.backend().list_tools(&scope).await.is_err(),
        "settled attempts must not keep authenticating"
    );
}

// ----- Fix B: fail closed when the owned MCP server is not connected ---

/// Runs one OwnedMcpChat turn whose scripted init frame is `init_line`.
async fn run_owned_mcp(h: &Harness, init_line: &str) -> HostResult<()> {
    h.shared.queue(&[init_line]);
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
    h.executor()
        .execute(h.context(&claimed, CancellationToken::new(), CancellationToken::new()))
        .await
}

async fn assert_bridge_failure(h: &Harness, err: HostError, reported: &str) {
    let message = Harness::failure_message(&err);
    let (code, _) = Harness::failure(err);
    assert_eq!(code.as_deref(), Some("native_mcp_bridge_unavailable"));
    assert!(
        message.contains("onemind") && message.contains(reported),
        "{message}"
    );
    let types = h.event_types("t1").await;
    assert!(
        types.iter().any(|t| t == "native.event"),
        "evidence is durable: {types:?}"
    );
    assert!(types.iter().any(|t| t == "native.failed"), "{types:?}");
    assert!(!types.iter().any(|t| t == "native.completed"), "{types:?}");
    let failed = h.event_data("t1", "native.failed").await.unwrap();
    assert_eq!(failed["class"], "mcp_bridge_unavailable");
    assert_eq!(
        *h.shared.interrupts.lock().unwrap(),
        [InterruptPolicy::Quarantine]
    );
}

#[tokio::test]
async fn owned_mcp_fails_closed_when_the_server_reports_failed() {
    let h = harness();
    // Even a scripted `Completed` must not win over the bridge failure.
    h.shared
        .set_outcome(Shared::completed("vendor-1", "fabricated"));
    let err = run_owned_mcp(
        &h,
        r#"{"e":"init","s":"vendor-1","mcp":{"onemind":"failed"}}"#,
    )
    .await
    .unwrap_err();
    assert_bridge_failure(&h, err, "failed").await;
}

#[tokio::test]
async fn owned_mcp_fails_closed_when_the_server_is_not_reported() {
    let h = harness();
    h.shared
        .set_outcome(Shared::completed("vendor-1", "fabricated"));
    let err = run_owned_mcp(
        &h,
        r#"{"e":"init","s":"vendor-1","mcp":{"someone-else":"connected"}}"#,
    )
    .await
    .unwrap_err();
    assert_bridge_failure(&h, err, "not reported").await;
}

#[tokio::test]
async fn owned_mcp_completes_when_the_server_is_connected() {
    let h = harness();
    h.shared
        .set_outcome(Shared::completed("vendor-1", "real answer"));
    run_owned_mcp(
        &h,
        r#"{"e":"init","s":"vendor-1","mcp":{"onemind":"connected"}}"#,
    )
    .await
    .expect("a connected bridge completes as before");
    let types = h.event_types("t1").await;
    assert!(types.iter().any(|t| t == "native.completed"), "{types:?}");
    assert!(h.shared.interrupts.lock().unwrap().is_empty());
}

#[tokio::test]
async fn plain_chat_without_mcp_servers_completes_as_before() {
    let h = harness();
    h.shared.queue(&[r#"{"e":"init","s":"vendor-1"}"#]);
    h.shared.set_outcome(Shared::completed("vendor-1", "plain"));
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
        .expect("plain chat is unaffected by the MCP gate");
    assert!(h.shared.interrupts.lock().unwrap().is_empty());
}

// ----- Fix C: stderr surfaces in exit-derived failures ------------------

const INVALID_SESSION: &str = "Error: Invalid session ID. Must be a valid UUID.";

#[tokio::test]
async fn early_exit_failure_carries_the_stderr_tail() {
    let h = harness();
    h.shared.exit_code.store(1, Ordering::SeqCst);
    *h.shared.stderr.lock().unwrap() = format!("\x1b[31m{INVALID_SESSION}\x1b[0m\n").into_bytes();
    h.shared.set_outcome(NativeTerminalOutcome::Failed {
        class: FailureClass::ExitWithoutResult,
        message: "vendor process exited without a root result (exit_code=Some(1), signal=None)"
            .into(),
        partial_text: None,
    });
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
    let message = Harness::failure_message(&err);
    assert!(message.contains("exit_code=Some(1)"), "{message}");
    assert!(
        message.contains(&format!("; stderr: [31m{INVALID_SESSION}[0m")),
        "{message}"
    );
    let failed = h.event_data("t1", "native.failed").await.unwrap();
    let tail = failed["stderrTail"].as_str().unwrap();
    assert!(
        tail.contains(INVALID_SESSION) && !tail.contains('\u{1b}'),
        "{tail}"
    );
}

#[tokio::test]
async fn uncertain_exit_carries_the_stderr_tail_and_completed_does_not() {
    let h = harness();
    *h.shared.stderr.lock().unwrap() = INVALID_SESSION.as_bytes().to_vec();
    // No scripted outcome: the fake adapter reports Uncertain/ExitWithoutResult.
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
    assert!(Harness::failure_message(&err).ends_with(&format!("; stderr: {INVALID_SESSION}")));
    let uncertain = h.event_data("t1", "native.uncertain").await.unwrap();
    assert_eq!(uncertain["stderrTail"], INVALID_SESSION);
    assert!(
        uncertain["detail"]
            .as_str()
            .unwrap()
            .contains(INVALID_SESSION)
    );

    // A completed turn never gets the tail, even if stderr is noisy.
    let h = harness();
    *h.shared.stderr.lock().unwrap() = b"warning noise".to_vec();
    h.shared.queue(&[r#"{"e":"init","s":"vendor-1"}"#]);
    h.shared.set_outcome(Shared::completed("vendor-1", "ok"));
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
        .unwrap();
    let completed = h.event_data("t1", "native.completed").await.unwrap();
    assert!(completed.get("stderrTail").is_none());
}

#[tokio::test]
async fn plain_chat_with_broker_payload_is_refused() {
    let h = harness();
    h.submit(
        "t1",
        &payload(
            &native_target(NativeProfile::PlainChat, false),
            &fresh_input(),
            Some(&broker_spec()),
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
    assert_eq!(code.as_deref(), Some("broker_not_allowed"));
}
