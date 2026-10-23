use crate::{CoreError, Runtime};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BudgetLimits {
    pub first_byte_ms: Option<u64>,
    pub stream_idle_ms: Option<u64>,
    pub tool_ms: Option<u64>,
    pub overall_ms: Option<u64>,
    pub provider_requests: Option<u64>,
    pub tool_calls: Option<u64>,
    pub correction_passes: Option<u64>,
    pub retries: Option<u64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BudgetSnapshot {
    pub version: u8,
    pub started_at: u64,
    pub provider_requests: u64,
    pub tool_calls: u64,
    pub correction_passes: u64,
    pub retries: u64,
}
#[async_trait::async_trait]
pub trait BudgetCheckpoint: Send + Sync {
    async fn checkpoint(&self, state: BudgetSnapshot) -> Result<(), CoreError>;
}
pub struct ExecutionBudget {
    pub limits: BudgetLimits,
    state: Mutex<BudgetSnapshot>,
    runtime: Arc<dyn Runtime>,
    checkpoint: Option<Arc<dyn BudgetCheckpoint>>,
    consume_lock: tokio::sync::Mutex<()>,
    failure: Mutex<Option<CoreError>>,
}
impl ExecutionBudget {
    pub fn new(
        limits: BudgetLimits,
        runtime: Arc<dyn Runtime>,
        state: Option<BudgetSnapshot>,
    ) -> Result<Self, CoreError> {
        let state = state.unwrap_or(BudgetSnapshot {
            version: 1,
            started_at: runtime.now_ms(),
            provider_requests: 0,
            tool_calls: 0,
            correction_passes: 0,
            retries: 0,
        });
        if state.version != 1 {
            return Err(CoreError::new(
                "invalid_budget",
                "Invalid execution budget state",
            ));
        }
        Ok(Self {
            limits,
            state: Mutex::new(state),
            runtime,
            checkpoint: None,
            consume_lock: tokio::sync::Mutex::new(()),
            failure: Mutex::new(None),
        })
    }
    pub fn with_checkpoint(mut self, checkpoint: Arc<dyn BudgetCheckpoint>) -> Self {
        self.checkpoint = Some(checkpoint);
        self
    }
    pub fn snapshot(&self) -> BudgetSnapshot {
        self.state.lock().expect("budget lock poisoned").clone()
    }
    pub fn remaining_ms(&self) -> Option<u64> {
        self.limits.overall_ms.map(|limit| {
            self.snapshot()
                .started_at
                .saturating_add(limit)
                .saturating_sub(self.runtime.now_ms())
        })
    }
    pub fn fail(&self, error: CoreError) {
        *self.failure.lock().expect("budget lock poisoned") = Some(error);
    }
    pub fn check(&self) -> Result<(), CoreError> {
        if let Some(error) = self.failure.lock().expect("budget lock poisoned").clone() {
            return Err(error);
        }
        if self.remaining_ms() == Some(0) {
            return Err(timeout("overall"));
        }
        Ok(())
    }
    pub async fn consume(&self, counter: &str) -> Result<(), CoreError> {
        let _serial = self.consume_lock.lock().await;
        self.check()?;
        let state = self.reserve(counter)?;
        if let Some(checkpoint) = &self.checkpoint {
            if let Err(error) = checkpoint.checkpoint(state).await {
                *self.failure.lock().expect("budget lock poisoned") = Some(error.clone());
                return Err(error);
            }
        }
        self.check()
    }
    fn reserve(&self, counter: &str) -> Result<BudgetSnapshot, CoreError> {
        let mut state = self.state.lock().expect("budget lock poisoned");
        let (value, limit) = match counter {
            "providerRequests" => (&mut state.provider_requests, self.limits.provider_requests),
            "toolCalls" => (&mut state.tool_calls, self.limits.tool_calls),
            "correctionPasses" => (&mut state.correction_passes, self.limits.correction_passes),
            "retries" => (&mut state.retries, self.limits.retries),
            _ => {
                return Err(CoreError::new(
                    "invalid_budget",
                    "Unknown execution counter",
                ));
            }
        };
        if limit.is_some_and(|limit| *value >= limit) {
            return Err(CoreError::new(
                "execution_budget_exhausted",
                &format!("execution_budget_exhausted: {counter}"),
            ));
        }
        *value = value
            .checked_add(1)
            .filter(|value| *value <= 9007199254740991)
            .ok_or_else(|| {
                CoreError::new(
                    "execution_budget_exhausted",
                    "Execution counter exceeds safe integer range",
                )
            })?;
        Ok(state.clone())
    }
}
pub(crate) fn timeout(phase: &str) -> CoreError {
    CoreError::new("execution_timeout", &format!("execution_timeout: {phase}"))
}
pub(crate) fn minimum(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}
