use super::*;

#[test]
fn contract_is_valid_and_deduplicated() {
    let codes = contract_codes();
    assert!(!codes.is_empty());
    let unique: std::collections::HashSet<&String> = codes.iter().collect();
    assert_eq!(unique.len(), codes.len(), "codes must be deduplicated");
    assert!(codes.iter().any(|code| code == "internal_error"));
}

#[test]
fn contract_codes_cross_the_boundary_verbatim() {
    let error = stable("answer_conflict", "duplicate answer", None);
    let payload: Value = serde_json::from_str(&error.reason).expect("payload is JSON");
    assert_eq!(payload["code"], "answer_conflict");
    assert_eq!(payload["message"], "duplicate answer");
    assert!(payload.get("details").is_none());
}

#[test]
fn coded_runtime_errors_keep_the_old_pack_mapping() {
    let known = runtime(RuntimeError::Coded {
        code: "invalid_input".to_owned(),
        message: "nope".to_owned(),
    });
    let payload: Value = serde_json::from_str(&known.reason).expect("payload is JSON");
    assert_eq!(payload["code"], "invalid_input");
    assert_eq!(payload["message"], "nope");
    // A contract code crosses verbatim (workspace_busy is one).
    let verbatim = runtime(RuntimeError::Coded {
        code: "workspace_busy".to_owned(),
        message: "leased".to_owned(),
    });
    let payload: Value = serde_json::from_str(&verbatim.reason).expect("payload is JSON");
    assert_eq!(payload["code"], "workspace_busy");
    // An unknown origin is redacted to internal_error but stays
    // inspectable in details.origin.
    let foreign = runtime(RuntimeError::Coded {
        code: "not_in_the_contract".to_owned(),
        message: "x".to_owned(),
    });
    let payload: Value = serde_json::from_str(&foreign.reason).expect("payload is JSON");
    assert_eq!(payload["code"], "internal_error");
    assert_eq!(payload["message"], GENERIC_MESSAGE);
    assert_eq!(payload["details"]["origin"], "not_in_the_contract");
}

#[test]
fn answer_stale_crosses_the_boundary_verbatim() {
    // The stale-answer code is part of the contract: a session-service
    // stale refusal must reach JavaScript as `answer_stale` with the
    // TypeScript message text, never redacted to `internal_error`.
    let message = "Request req-1 can no longer be answered: it was cancelled.";
    let error = session(agentkit_sessions::SessionsError::AnswerStale(
        message.into(),
    ));
    let payload: Value = serde_json::from_str(&error.reason).expect("payload is JSON");
    assert_eq!(payload["code"], "answer_stale");
    assert_eq!(payload["message"], message);
    assert!(payload.get("details").is_none());
    assert!(is_contract_code("answer_stale"));
}

#[test]
fn unknown_codes_are_redacted_not_leaked() {
    let error = stable("weird-debug-code", "raw internal detail", None);
    let payload: Value = serde_json::from_str(&error.reason).expect("payload is JSON");
    assert_eq!(payload["code"], "internal_error");
    // The raw message NEVER crosses for a non-contract code: only the
    // fixed generic string does, and the unknown code survives solely
    // as an origin marker, never as the wire code.
    let message = payload["message"].as_str().expect("message is a string");
    assert!(!message.contains("raw internal detail"));
    assert_eq!(message, GENERIC_MESSAGE);
    assert_eq!(payload["details"]["origin"], "weird-debug-code");
}

#[test]
fn explicit_internal_error_messages_are_redacted() {
    // Even a caller that names the contract code cannot smuggle raw
    // runtime text across: the message is the fixed generic string.
    let error = stable("internal_error", "raw secret", None);
    let payload: Value = serde_json::from_str(&error.reason).expect("payload is JSON");
    assert_eq!(payload["code"], "internal_error");
    let message = payload["message"].as_str().expect("message is a string");
    assert!(!message.contains("raw secret"));
    assert_eq!(message, GENERIC_MESSAGE);
}

#[test]
fn internal_keeps_its_compile_time_context() {
    // `internal` bypasses the blanket redaction because every call site
    // passes a compile-time literal (audited), so the context message
    // stays observable.
    let error = internal("run event serialization");
    let payload: Value = serde_json::from_str(&error.reason).expect("payload is JSON");
    assert_eq!(payload["code"], "internal_error");
    assert_eq!(
        payload["message"],
        "run event serialization failed; inspect the native logs"
    );
    assert!(payload.get("details").is_none());
}

#[test]
fn host_store_errors_keep_their_store_codes_and_unconfigured_lanes_are_stable() {
    let payload = |error: Error| -> Value { serde_json::from_str(&error.reason).unwrap() };
    let mapped = payload(host(agentkit_engine::EngineError::Store(
        agentkit_store::StoreError::NotFound("x".into()),
    )));
    assert_eq!(mapped["code"], "not_found");
    let mapped = payload(host(agentkit_engine::EngineError::Store(
        agentkit_store::StoreError::LeaseLost("x".into()),
    )));
    assert_eq!(mapped["code"], "lease_lost");
    let mapped = payload(agents(AgentsError::SubagentsNotConfigured));
    assert_eq!(mapped["code"], "subagents_not_configured");
    assert_eq!(mapped["message"], "subagents not configured");
    let mapped = payload(agents(AgentsError::AdvisorNotConfigured));
    assert_eq!(mapped["code"], "advisor_not_configured");
    assert_eq!(mapped["message"], "advisor not configured");
    let mapped = payload(agents(AgentsError::SubagentNotFound("c1".into())));
    assert_eq!(mapped["code"], "not_found");
    assert_eq!(mapped["message"], "subagent_not_found: c1");
}

#[test]
fn host_error_text_is_redacted_not_leaked() {
    // `host` hands the Display text of an engine error to `stable` with the
    // internal_error code; the redaction now keeps it native-only.
    let error = host(agentkit_engine::EngineError::External {
        source: Box::new(std::io::Error::other(
            "tcp connect to 10.0.0.7:8080 refused: token abc123",
        )),
        failure_code: None,
    });
    let payload: Value = serde_json::from_str(&error.reason).expect("payload is JSON");
    assert_eq!(payload["code"], "internal_error");
    let message = payload["message"].as_str().expect("message is a string");
    assert_eq!(message, GENERIC_MESSAGE);
    assert!(!error.reason.contains("10.0.0.7"));
    assert!(!error.reason.contains("abc123"));
}

#[test]
fn store_unknown_fallback_keeps_origin_and_redacts_message() {
    // Variants with no explicit arm land in `unknown-store-error`: not a
    // contract code, so the wire code is internal_error with the origin
    // preserved and the Display text withheld.
    let error = store(agentkit_store::StoreError::SessionDescriptorChanged(
        "raw descriptor diff leaked".into(),
    ));
    let payload: Value = serde_json::from_str(&error.reason).expect("payload is JSON");
    assert_eq!(payload["code"], "internal_error");
    assert_eq!(payload["details"]["origin"], "unknown-store-error");
    assert_eq!(payload["message"], GENERIC_MESSAGE);
    assert!(!error.reason.contains("raw descriptor diff leaked"));
}

#[test]
fn store_variants_map_to_contract_codes() {
    let cases = [
        (agentkit_store::StoreError::AlreadyOwned, "already_owned"),
        (agentkit_store::StoreError::Closed, "store_closed"),
        (
            agentkit_store::StoreError::NotFound("x".into()),
            "not_found",
        ),
        (
            agentkit_store::StoreError::RecoveryUnreadable("x".into()),
            "recovery_unreadable",
        ),
        (
            agentkit_store::StoreError::UnsupportedVersion(9),
            "unsupported_version",
        ),
    ];
    for (error, expected) in cases {
        let payload: Value = serde_json::from_str(&store(error).reason).expect("JSON");
        assert_eq!(payload["code"], expected, "for {expected}");
    }
}

#[test]
fn general_and_teammate_contexts_classify_typed_refusals_and_spoofs() {
    use agentkit_sessions::SessionsError as S;
    let payload = |error: Error| -> Value { serde_json::from_str(&error.reason).unwrap() };
    let general = payload(sessions(S::InvalidOverrides("bad".into())));
    assert_eq!(general["code"], "internal_error");
    assert_eq!(general["message"], GENERIC_MESSAGE);
    let teammate = payload(super::teammate(S::InvalidOverrides("bad".into())));
    assert_eq!(
        teammate,
        json!({"code":"invalid_input","message":"invalid_overrides: bad"})
    );
    for text in ["invalid_overrides: secret", "run_profile_refused: secret"] {
        let wire = payload(super::teammate(S::ContentAuthorityCallback(text.into())));
        assert_eq!(
            wire,
            json!({"code":"internal_error","message":GENERIC_MESSAGE})
        );
    }
    let missing = payload(sessions(S::ReceiptNotFound {
        execution_id: "e".into(),
        chat_id: "c".into(),
    }));
    assert_eq!(
        missing,
        json!({"code":"not_found","message":"not_found: execution e not found in chat c."})
    );
}

#[test]
fn nested_store_and_managed_reasons_preserve_wire_context() {
    use agentkit_store::{ManagedInputReason as R, StoreError as E};
    let payload = |error: Error| -> Value { serde_json::from_str(&error.reason).unwrap() };
    let nested = agentkit_chat::ChatError::Sessions(agentkit_sessions::SessionsError::Store(
        E::LeaseLost("r".into()),
    ));
    assert_eq!(payload(chat(nested))["code"], "lease_lost");
    let typed = E::ManagedInput {
        reason: R::SecretReferencesUnsupported,
        message: "secret details".into(),
    };
    assert_eq!(payload(managed_store(typed))["code"], "mcp_secret_missing");
    let spoof = E::InvalidInput("secret_ref_missing: spoof".into());
    assert_eq!(payload(managed_store(spoof))["code"], "invalid_input");
}
