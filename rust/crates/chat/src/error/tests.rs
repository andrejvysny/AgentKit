use super::*;
use agentkit_engine::{ExecutionErrorKind, classify_execution_error};
use agentkit_governance::GovernanceError;
use std::error::Error;

#[test]
fn typed_and_external_boundaries_preserve_classification_and_sources() {
    let cases = [
        (
            ChatError::Governance(GovernanceError::ToolNamespaceInvalid),
            ExecutionErrorKind::Terminal,
            "unclassified",
        ),
        (
            ChatError::SessionContextInvalid,
            ExecutionErrorKind::Terminal,
            "rejected",
        ),
        (
            ChatError::ReadInstructionFile(std::io::Error::other("permission denied")),
            ExecutionErrorKind::Terminal,
            "rejected",
        ),
        (
            ChatError::ReadReceipts(StoreError::Io(std::io::Error::other("ETIMEDOUT"))),
            ExecutionErrorKind::Transient,
            "network:ETIMEDOUT",
        ),
    ];
    for (error, kind, reason) in cases {
        let engine: agentkit_engine::EngineError<StoreError> = error.into();
        let classification = classify_execution_error(&engine);
        assert_eq!(
            (classification.kind, classification.reason.as_str()),
            (kind, reason)
        );
        assert!(engine.source().is_some());
        assert!(!matches!(engine, agentkit_engine::EngineError::Failure(_)));
    }
}

#[test]
fn boundary_unwraps_nested_cancellation_store_and_failure_metadata() {
    use agentkit_engine::{EngineError, ExecutionFailure, RetrySafety};
    use agentkit_sessions::SessionsError;
    for normalized in [false, true] {
        let wrap = |error| {
            if normalized {
                ChatError::from(error)
            } else {
                ChatError::Sessions(error)
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
            message: "transient".into(),
            code: Some("provider_error".into()),
            retryable: Some(true),
            http_status: Some(503),
            retry_safety: RetrySafety::SafeToRetry,
        };
        let converted: EngineError<StoreError> =
            wrap(SessionsError::Engine(EngineError::Failure(failure))).into();
        let EngineError::Failure(failure) = converted else {
            panic!("lost failure");
        };
        assert_eq!(failure.message, "transient");
        assert_eq!(
            (
                failure.code.as_deref(),
                failure.retryable,
                failure.http_status,
                failure.retry_safety
            ),
            (
                Some("provider_error"),
                Some(true),
                Some(503),
                RetrySafety::SafeToRetry
            )
        );
    }
}

#[test]
fn opaque_diagnostics_cannot_mint_owned_failure_event_codes() {
    for message in ["invalid_budget: spoof", "hook_timeout: spoof"] {
        let error = ChatError::External(Box::new(std::io::Error::other(message)));
        assert_eq!(error.failure_event_code(), "host_error");
    }
    assert_eq!(
        ChatError::InvalidBudget("too large".into()).failure_event_code(),
        "invalid_budget"
    );
    assert_eq!(
        ChatError::HookTimeout("context".into()).failure_event_code(),
        "hook_timeout"
    );
}

#[test]
fn session_callbacks_keep_external_classification_and_refusals_are_fixed() {
    use agentkit_engine::EngineError;
    use agentkit_sessions::SessionsError as S;
    for (error, kind, reason) in [
        (
            S::ContentAuthorityCallback("ETIMEDOUT".into()),
            ExecutionErrorKind::Transient,
            "network:ETIMEDOUT",
        ),
        (
            S::SessionStore(agentkit_sessions::SessionStoreError(StoreError::Io(
                std::io::Error::other("ETIMEDOUT"),
            ))),
            ExecutionErrorKind::Transient,
            "network:ETIMEDOUT",
        ),
        (
            S::InvalidProviderId,
            ExecutionErrorKind::Terminal,
            "rejected",
        ),
        (
            S::InvalidProviderSnapshot,
            ExecutionErrorKind::Terminal,
            "rejected",
        ),
        (
            S::ProviderCancelled,
            ExecutionErrorKind::Terminal,
            "unclassified",
        ),
    ] {
        let engine: EngineError<StoreError> = ChatError::Sessions(error).into();
        let classification = classify_execution_error(&engine);
        assert_eq!(
            (classification.kind, classification.reason.as_str()),
            (kind, reason)
        );
        assert!(!matches!(engine, EngineError::Cancelled));
    }
}

#[test]
fn engine_boundary_retains_typed_and_external_failure_codes() {
    use agentkit_engine::EngineError;
    use agentkit_sessions::{SessionStoreError, SessionsError};
    let cases = [
        (ChatError::InvalidBudget("large".into()), "invalid_budget"),
        (
            ChatError::Sessions(SessionsError::InvalidProfile("bad".into())),
            "invalid_profile",
        ),
        (
            ChatError::Core(agentkit_core::CoreError::new("provider_code", "detail")),
            "provider_code",
        ),
        (
            ChatError::Sessions(SessionsError::ProviderCore(agentkit_core::CoreError::new(
                "provider_code",
                "detail",
            ))),
            "provider_code",
        ),
        (
            ChatError::Sessions(SessionsError::SessionStore(SessionStoreError(
                StoreError::Io(std::io::Error::other("ETIMEDOUT")),
            ))),
            "store_failure",
        ),
        (
            ChatError::ReadInstructionFile(std::io::Error::other("private")),
            "Cannot read instruction file",
        ),
        (
            ChatError::ReadReceipts(StoreError::Io(std::io::Error::other("private"))),
            "Cannot read durable execution receipts; mutations remain blocked. (filesystem error",
        ),
        (
            ChatError::ReadReceipts(StoreError::LeaseLost("turn:input".into())),
            "Cannot read durable execution receipts; mutations remain blocked. (lease lost for task turn",
        ),
    ];
    for (error, expected) in cases {
        let engine: EngineError<StoreError> = error.into();
        assert_eq!(ChatError::Engine(engine).failure_event_code(), expected);
    }
}
