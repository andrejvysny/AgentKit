use super::*;
use agentkit_engine::{ExecutionFailure, RetrySafety};
use agentkit_sessions::SessionsError;
use std::error::Error;

#[test]
fn nested_context_boundaries_preserve_engine_semantics() {
    for chat_wrapper in [false, true] {
        let wrap = |error| {
            if chat_wrapper {
                AgentsError::Chat(agentkit_chat::ChatError::Sessions(error))
            } else {
                AgentsError::Sessions(error)
            }
        };
        let cancel: EngineError<StoreError> =
            wrap(SessionsError::Engine(EngineError::Cancelled)).into();
        assert!(matches!(cancel, EngineError::Cancelled));
        let lease: EngineError<StoreError> = wrap(SessionsError::Engine(EngineError::Store(
            StoreError::LeaseLost("run".into()),
        )))
        .into();
        assert!(matches!(
            lease,
            EngineError::Store(StoreError::LeaseLost(_))
        ));
        let failure = ExecutionFailure {
            message: "temporary".into(),
            code: Some("provider".into()),
            retryable: Some(true),
            http_status: Some(503),
            retry_safety: RetrySafety::SafeToRetry,
        };
        let failure: EngineError<StoreError> =
            wrap(SessionsError::Engine(EngineError::Failure(failure))).into();
        let EngineError::Failure(failure) = failure else {
            panic!("lost failure");
        };
        assert_eq!(
            (
                failure.message.as_str(),
                failure.code.as_deref(),
                failure.retryable,
                failure.http_status,
                failure.retry_safety
            ),
            (
                "temporary",
                Some("provider"),
                Some(true),
                Some(503),
                RetrySafety::SafeToRetry
            )
        );
    }
}

#[test]
fn advisor_budget_error_preserves_store_source_and_display() {
    let error = AgentsError::AdvisorBudgetUnavailable(StoreError::LeaseLost("run".into()));
    assert_eq!(
        error.to_string(),
        "advisor_rate_limited: review budget unavailable: lease lost for task run"
    );
    assert!(error.source().unwrap().is::<StoreError>());
}

#[test]
fn context_event_codes_keep_identity_prefixes_and_sources() {
    let cases = [
        (
            AgentsError::Core(CoreError::new("advisor_disabled", "opaque")),
            "advisor_disabled",
        ),
        (
            AgentsError::AdvisorBudgetUnavailable(StoreError::Io(std::io::Error::other(
                "ETIMEDOUT",
            ))),
            "advisor_rate_limited",
        ),
        (AgentsError::RoleCannotSpawn("role:sub".into()), "role role"),
        (
            AgentsError::SpawnIdentityConflict("spawn:key".into()),
            "spawn key spawn",
        ),
        (
            AgentsError::ChildSessionConflict("child:id".into()),
            "child session id child",
        ),
    ];
    for (error, expected) in cases {
        let engine: EngineError<StoreError> = error.into();
        let code = match &engine {
            EngineError::External { failure_code, .. }
            | EngineError::Context { failure_code, .. } => failure_code.as_deref(),
            _ => panic!("expected context boundary"),
        };
        assert_eq!(code, Some(expected));
        assert!(engine.source().is_some());
    }
}
