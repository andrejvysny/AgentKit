//! Classification follows the TypeScript confidence order. Scheduling adds an
//! explicit replay-safety gate: a socket or HTTP failure cannot prove a domain
//! write did not commit. Unknown outcomes remain terminal even when transient.
//! SafeToRetry requires evidence that an effect did not land or is idempotent.
use crate::{HostError, HostResult};

/// A transient failure does not by itself prove that replaying an effect is safe.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RetrySafety {
    SafeToRetry,
    #[default]
    UnknownOutcome,
}

#[derive(Clone, Debug, thiserror::Error)]
#[error("{message}")]
pub struct ExecutionFailure {
    pub message: String,
    pub code: Option<String>,
    pub retryable: Option<bool>,
    pub http_status: Option<u16>,
    pub retry_safety: RetrySafety,
}

impl ExecutionFailure {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: None,
            retryable: None,
            http_status: None,
            retry_safety: RetrySafety::UnknownOutcome,
        }
    }

    /// The executor must have evidence that an effect did not land or is idempotent.
    pub fn safe_to_retry(message: impl Into<String>) -> Self {
        Self {
            retryable: Some(true),
            retry_safety: RetrySafety::SafeToRetry,
            ..Self::new(message)
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecutionErrorKind {
    Transient,
    Terminal,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutionErrorClassification {
    pub kind: ExecutionErrorKind,
    pub reason: String,
}

fn classification(
    kind: ExecutionErrorKind,
    reason: impl Into<String>,
) -> ExecutionErrorClassification {
    ExecutionErrorClassification {
        kind,
        reason: reason.into(),
    }
}

fn host_code(code: &str) -> Option<ExecutionErrorKind> {
    use ExecutionErrorKind::*;
    match code {
        "lease_lost" => Some(Transient),
        "seq_conflict"
        | "invalid_task_transition"
        | "invalid_proposal_transition"
        | "duplicate_action_id"
        | "duplicate_task"
        | "executor_not_found"
        | "invalid_task_payload"
        | "unknown_dependency"
        | "revision_conflict"
        | "not_found"
        | "invalid_fork_point" => Some(Terminal),
        _ => None,
    }
}

/// Matches the TypeScript classifier; scheduling additionally requires explicit replay safety.
pub fn classify_execution_error(error: &HostError) -> ExecutionErrorClassification {
    use ExecutionErrorKind::*;
    if matches!(error, HostError::Cancelled) {
        return classification(Cancelled, "aborted");
    }
    if let Some(result) = classify_host_error(error) {
        return result;
    }
    let HostError::Failure(failure) = error else {
        return classify_message(&error.to_string(), None);
    };
    if failure.code.as_deref() == Some("ABORT_ERR") {
        return classification(Cancelled, "aborted");
    }
    if let Some(code) = failure.code.as_deref()
        && let Some(kind) = host_code(code)
    {
        return classification(kind, code);
    }
    if let Some(retryable) = failure.retryable {
        return classification(
            if retryable { Transient } else { Terminal },
            if retryable {
                "retryable_flag"
            } else {
                "non_retryable_flag"
            },
        );
    }
    if let Some(status) = failure.http_status {
        if status == 429 || status >= 500 {
            return classification(Transient, format!("http_{status}"));
        }
        if status >= 400 {
            return classification(Terminal, format!("http_{status}"));
        }
    }
    classify_message(&failure.message, failure.code.as_deref())
}

fn classify_host_error(error: &HostError) -> Option<ExecutionErrorClassification> {
    use agentkit_store::StoreError;
    let code = match error {
        HostError::UnknownKind(_) => "executor_not_found",
        HostError::Store(StoreError::LeaseLost(_)) => "lease_lost",
        HostError::Store(StoreError::InvalidTransition(_)) => "invalid_task_transition",
        HostError::Store(StoreError::NotFound(_)) => "not_found",
        HostError::Store(StoreError::Duplicate(_)) => "duplicate_task",
        HostError::Store(StoreError::InvalidInput(message))
            if message.starts_with("unknown dependency:") =>
        {
            "unknown_dependency"
        }
        HostError::Store(StoreError::InvalidInput(message))
            if message.starts_with("event seq must be") =>
        {
            "seq_conflict"
        }
        HostError::Store(StoreError::InvalidInput(message)) if message == "invalid_fork_point" => {
            "invalid_fork_point"
        }
        _ => return None,
    };
    host_code(code).map(|kind| classification(kind, code))
}

fn classify_message(message: &str, code: Option<&str>) -> ExecutionErrorClassification {
    use ExecutionErrorKind::*;
    for marker in [
        "ECONNRESET",
        "ECONNREFUSED",
        "ECONNABORTED",
        "ETIMEDOUT",
        "EPIPE",
        "EHOSTUNREACH",
        "ENETUNREACH",
        "EAI_AGAIN",
    ] {
        if code == Some(marker) || message.contains(marker) {
            return classification(Transient, format!("network:{marker}"));
        }
    }
    let lowered = message.to_lowercase();
    if [
        "fetch failed",
        "socket",
        "network error",
        "connection reset",
        "connection refused",
        "timed out",
    ]
    .iter()
    .any(|needle| lowered.contains(needle))
    {
        return classification(Transient, "network");
    }
    let numbers = message
        .split(|character: char| !character.is_alphanumeric() && character != '_')
        .collect::<Vec<_>>();
    if numbers.iter().any(|token| {
        token.len() == 3
            && token.starts_with('5')
            && token.bytes().all(|byte| byte.is_ascii_digit())
    }) {
        return classification(Transient, "http_5xx");
    }
    if numbers.contains(&"429") {
        return classification(Transient, "http_429");
    }
    if numbers
        .iter()
        .any(|number| ["400", "401", "403", "404", "422"].contains(number))
    {
        return classification(Terminal, "http_rejected");
    }
    if [
        "unauthorized",
        "invalid",
        "forbidden",
        "authentication",
        "permission denied",
        "not permitted",
        "bad request",
        "unprocessable",
    ]
    .iter()
    .any(|needle| lowered.contains(needle))
    {
        return classification(Terminal, "rejected");
    }
    classification(Terminal, "unclassified")
}

pub(crate) fn retry_is_safe(error: &HostError) -> bool {
    matches!(error,HostError::Failure(failure) if failure.retry_safety==RetrySafety::SafeToRetry)
}

#[derive(Clone, Debug)]
pub struct RetryBackoff {
    pub base_ms: u64,
    pub max_ms: u64,
    pub jitter_ratio: f64,
}

impl Default for RetryBackoff {
    fn default() -> Self {
        Self {
            base_ms: 1000,
            max_ms: 30000,
            jitter_ratio: 0.2,
        }
    }
}

impl RetryBackoff {
    pub(crate) fn validate(&self) -> HostResult<()> {
        if self.base_ms > i64::MAX as u64
            || self.max_ms > i64::MAX as u64 / 2
            || !self.jitter_ratio.is_finite()
            || !(0.0..=1.0).contains(&self.jitter_ratio)
        {
            return Err(HostError::Execution("invalid retry backoff".into()));
        }
        Ok(())
    }

    pub fn delay_ms(&self, attempt_count: u32, random_unit: f64) -> HostResult<i64> {
        self.validate()?;
        if !random_unit.is_finite() || !(0.0..=1.0).contains(&random_unit) {
            return Err(HostError::Execution("invalid retry jitter sample".into()));
        }
        if self.base_ms == 0 {
            return Ok(0);
        }
        let exponent = attempt_count.saturating_sub(1);
        let exponential = if exponent >= 64 {
            u64::MAX
        } else {
            self.base_ms.saturating_mul(1u64 << exponent)
        };
        let capped = exponential.min(self.max_ms) as f64;
        Ok(
            (capped + capped * self.jitter_ratio * (random_unit * 2.0 - 1.0))
                .max(0.0)
                .ceil() as i64,
        )
    }
}

#[cfg(test)]
mod tests;
