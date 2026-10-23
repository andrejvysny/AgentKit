use super::*;

fn failure(
    code: Option<&str>,
    retryable: Option<bool>,
    status: Option<u16>,
    message: &str,
) -> HostError {
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
        (HostError::Cancelled, Cancelled, "aborted"),
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
    assert!(retry_is_safe(&HostError::from(
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
        assert_eq!(exact.delay_ms(attempt, 0.5).unwrap(), delay);
    }
    let jitter = RetryBackoff::default();
    assert_eq!(jitter.delay_ms(1, 0.0).unwrap(), 800);
    assert_eq!(jitter.delay_ms(1, 1.0).unwrap(), 1200);
    assert_eq!(jitter.delay_ms(1, 1.0 / 3.0).unwrap(), 934);
    assert_eq!(jitter.delay_ms(99, 1.0).unwrap(), 36000);
    assert_eq!(
        RetryBackoff {
            base_ms: 0,
            ..Default::default()
        }
        .delay_ms(u32::MAX, 0.5)
        .unwrap(),
        0
    );
    assert!(jitter.delay_ms(1, f64::NAN).is_err());
    assert!(
        RetryBackoff {
            jitter_ratio: 2.0,
            ..Default::default()
        }
        .delay_ms(1, 0.5)
        .is_err()
    );
}
