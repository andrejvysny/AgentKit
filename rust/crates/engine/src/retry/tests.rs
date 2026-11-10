use super::*;

/// The repository fault type is irrelevant to classification of non-store
/// errors; this one never occurs.
#[derive(Debug, thiserror::Error)]
#[error("unused")]
struct NoFault;

impl RepositoryFault for NoFault {
    fn is_transient(&self) -> bool {
        false
    }
    fn is_duplicate(&self) -> bool {
        false
    }
    fn is_lease_lost(&self) -> bool {
        false
    }
    fn is_invalid_transition(&self) -> bool {
        false
    }
    fn is_not_found(&self) -> bool {
        false
    }
    fn invalid_input(&self) -> Option<&str> {
        None
    }
    fn task_input_reason(&self) -> Option<crate::TaskInputReason> {
        None
    }
    fn session_conflict(&self) -> Option<&str> {
        None
    }
}

type EngineError = super::EngineError<NoFault>;

fn failure(
    code: Option<&str>,
    retryable: Option<bool>,
    status: Option<u16>,
    message: &str,
) -> EngineError {
    ExecutionFailure {
        code: code.map(str::to_owned),
        retryable,
        http_status: status,
        ..ExecutionFailure::new(message)
    }
    .into()
}

#[test]
fn classifier_preserves_structured_signal_precedence() {
    use ExecutionErrorKind::*;
    for (error, kind, reason) in [
        (EngineError::Cancelled, Cancelled, "aborted"),
        (
            failure(Some("ABORT_ERR"), Some(true), Some(503), "cancel"),
            Cancelled,
            "aborted",
        ),
        (
            failure(Some("seq_conflict"), Some(true), Some(503), "bad seq"),
            Terminal,
            "seq_conflict",
        ),
        (
            failure(Some("lease_lost"), Some(false), Some(401), "old lease"),
            Transient,
            "lease_lost",
        ),
        (
            failure(None, Some(true), Some(401), "host override"),
            Transient,
            "retryable_flag",
        ),
        (
            failure(None, Some(false), Some(503), "host override"),
            Terminal,
            "non_retryable_flag",
        ),
        (
            failure(None, None, Some(429), "limited"),
            Transient,
            "http_429",
        ),
        (
            failure(None, None, Some(503), "unavailable"),
            Transient,
            "http_503",
        ),
        (
            failure(None, None, Some(401), "denied"),
            Terminal,
            "http_401",
        ),
    ] {
        assert_eq!(
            classify_execution_error(&error),
            classification(kind, reason)
        );
    }
}

#[test]
fn classifier_matches_network_http_and_default_terminal_heuristics() {
    use ExecutionErrorKind::*;
    for (code, message, kind, reason) in [
        (
            Some("ECONNRESET"),
            "broken",
            Transient,
            "network:ECONNRESET",
        ),
        (None, "request timed out", Transient, "network"),
        (None, "upstream returned 502", Transient, "http_5xx"),
        (None, "status 429", Transient, "http_429"),
        (None, "status 422", Terminal, "http_rejected"),
        (None, "permission denied", Terminal, "rejected"),
        (None, "token_503_suffix", Terminal, "unclassified"),
        (None, "unknown executor bug", Terminal, "unclassified"),
    ] {
        assert_eq!(
            classify_execution_error(&failure(code, None, None, message)),
            classification(kind, reason)
        );
    }
    assert!(!retry_is_safe(&failure(
        None,
        Some(true),
        None,
        "unknown commit"
    )));
    assert!(retry_is_safe(&EngineError::from(
        ExecutionFailure::safe_to_retry("read-only lookup")
    )));
}

#[test]
fn backoff_is_exponential_capped_jittered_and_overflow_safe() {
    let exact = RetryBackoff {
        jitter_ratio: 0.0,
        ..Default::default()
    };
    for (attempt, delay) in [
        (0, 1000),
        (1, 1000),
        (2, 2000),
        (3, 4000),
        (6, 30000),
        (u32::MAX, 30000),
    ] {
        assert_eq!(exact.delay_ms::<NoFault>(attempt, 0.5).unwrap(), delay);
    }
    let jitter = RetryBackoff::default();
    assert_eq!(jitter.delay_ms::<NoFault>(1, 0.0).unwrap(), 800);
    assert_eq!(jitter.delay_ms::<NoFault>(1, 1.0).unwrap(), 1200);
    assert_eq!(jitter.delay_ms::<NoFault>(1, 1.0 / 3.0).unwrap(), 934);
    assert_eq!(jitter.delay_ms::<NoFault>(99, 1.0).unwrap(), 36000);
    assert_eq!(
        RetryBackoff {
            base_ms: 0,
            ..Default::default()
        }
        .delay_ms::<NoFault>(u32::MAX, 0.5)
        .unwrap(),
        0
    );
    assert!(jitter.delay_ms::<NoFault>(1, f64::NAN).is_err());
    assert!(
        RetryBackoff {
            jitter_ratio: 2.0,
            ..Default::default()
        }
        .delay_ms::<NoFault>(1, 0.5)
        .is_err()
    );
}

#[test]
fn typed_host_failures_override_misleading_diagnostics() {
    use ExecutionErrorKind::*;
    for (error, kind, reason) in [
        (EngineError::Cancelled, Cancelled, "aborted"),
        (
            EngineError::UnknownKind("503 socket".into()),
            Terminal,
            "executor_not_found",
        ),
        (
            EngineError::NoExecutorForKind("ETIMEDOUT".into()),
            Terminal,
            "no_executor_for_kind",
        ),
        (
            failure(Some("not_found"), None, Some(503), "socket timeout"),
            Terminal,
            "not_found",
        ),
        (
            failure(Some("lease_lost"), None, Some(400), "permission denied"),
            Transient,
            "lease_lost",
        ),
    ] {
        assert_eq!(
            classify_execution_error(&error),
            classification(kind, reason)
        );
        assert!(!retry_is_safe(&error));
    }
}

#[test]
fn context_errors_keep_sources_and_ignore_misleading_diagnostics() {
    use std::error::Error;
    let source = std::io::Error::other("hook_timeout: ETIMEDOUT 503 cancelled");
    let error = EngineError::Context {
        source: Box::new(source),
        classification: crate::ContextClassification::Rejected,
        failure_code: None,
    };
    assert_eq!(
        error.source().unwrap().to_string(),
        "hook_timeout: ETIMEDOUT 503 cancelled"
    );
    assert_eq!(
        classify_execution_error(&error),
        classification(ExecutionErrorKind::Terminal, "rejected")
    );
    assert!(!retry_is_safe(&error));
    let external = EngineError::External {
        source: Box::new(std::io::Error::other("ETIMEDOUT")),
        failure_code: None,
    };
    assert_eq!(
        classify_execution_error(&external),
        classification(ExecutionErrorKind::Transient, "network:ETIMEDOUT")
    );
    assert!(!retry_is_safe(&external));
    let cancelled_text = EngineError::External {
        source: Box::new(std::io::Error::other("cancelled")),
        failure_code: None,
    };
    assert_eq!(
        classify_execution_error(&cancelled_text),
        classification(ExecutionErrorKind::Terminal, "unclassified")
    );
}

#[test]
fn failure_event_metadata_cannot_override_retry_classification() {
    let external = EngineError::External {
        source: Box::new(std::io::Error::other("cancelled")),
        failure_code: Some("ABORT_ERR".into()),
    };
    assert_eq!(
        classify_execution_error(&external),
        classification(ExecutionErrorKind::Terminal, "unclassified")
    );
    assert!(!retry_is_safe(&external));
    let context = EngineError::Context {
        source: Box::new(std::io::Error::other("ETIMEDOUT")),
        classification: crate::ContextClassification::Rejected,
        failure_code: Some("lease_lost".into()),
    };
    assert_eq!(
        classify_execution_error(&context),
        classification(ExecutionErrorKind::Terminal, "rejected")
    );
    assert!(!retry_is_safe(&context));
}

#[derive(Debug, thiserror::Error)]
#[error("invalid input: {message}")]
struct InputFault {
    message: String,
    reason: Option<crate::TaskInputReason>,
}

impl RepositoryFault for InputFault {
    fn is_transient(&self) -> bool {
        false
    }
    fn is_duplicate(&self) -> bool {
        false
    }
    fn is_lease_lost(&self) -> bool {
        false
    }
    fn is_invalid_transition(&self) -> bool {
        false
    }
    fn is_not_found(&self) -> bool {
        false
    }
    fn invalid_input(&self) -> Option<&str> {
        Some(&self.message)
    }
    fn task_input_reason(&self) -> Option<crate::TaskInputReason> {
        self.reason
    }
    fn session_conflict(&self) -> Option<&str> {
        None
    }
}

#[test]
fn typed_input_reason_wins_over_untrusted_diagnostic() {
    let broad = super::EngineError::Store(InputFault {
        message: "unknown dependency: ETIMEDOUT 503".into(),
        reason: None,
    });
    assert_eq!(
        classify_execution_error(&broad),
        classification(ExecutionErrorKind::Terminal, "rejected")
    );
    let typed = super::EngineError::Store(InputFault {
        message: "ETIMEDOUT 503".into(),
        reason: Some(crate::TaskInputReason::UnknownDependency),
    });
    assert_eq!(
        classify_execution_error(&typed),
        classification(ExecutionErrorKind::Terminal, "unknown_dependency")
    );
}
