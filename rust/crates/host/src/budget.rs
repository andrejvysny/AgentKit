use crate::{ExecutionContext, HostError, HostResult};
use agentkit_core::{
    BudgetCheckpoint, BudgetLimits, BudgetSnapshot, CoreError, ExecutionBudget, Runtime,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::Mutex;

const LIMIT_FIELDS: &[&str] = &[
    "firstByteMs",
    "streamIdleMs",
    "toolMs",
    "overallMs",
    "providerRequests",
    "toolCalls",
    "correctionPasses",
    "retries",
];
const SNAPSHOT_FIELDS: &[&str] = &[
    "version",
    "startedAt",
    "providerRequests",
    "toolCalls",
    "correctionPasses",
    "retries",
];

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PersistedBudget {
    version: u8,
    attempt_number: u32,
    limits: BudgetLimits,
    snapshot: BudgetSnapshot,
}

fn invalid(message: &str) -> HostError {
    HostError::Execution(format!("invalid_budget: {message}"))
}

fn known_fields(value: &Value, fields: &[&str]) -> HostResult<()> {
    let object = value
        .as_object()
        .ok_or_else(|| invalid("expected object"))?;
    if object.keys().any(|key| !fields.contains(&key.as_str())) {
        return Err(invalid("unknown budget field"));
    }
    Ok(())
}

pub fn parse_budget_limits(value: Option<&Value>) -> HostResult<BudgetLimits> {
    let Some(value) = value else {
        return Ok(BudgetLimits::default());
    };
    known_fields(value, LIMIT_FIELDS)?;
    serde_json::from_value(value.clone()).map_err(|_| invalid("invalid budget limits"))
}

fn decode(value: Value) -> HostResult<PersistedBudget> {
    known_fields(&value["limits"], LIMIT_FIELDS)?;
    known_fields(&value["snapshot"], SNAPSHOT_FIELDS)?;
    let record: PersistedBudget =
        serde_json::from_value(value).map_err(|_| invalid("malformed persisted budget"))?;
    validate(&record)?;
    Ok(record)
}

fn validate(record: &PersistedBudget) -> HostResult<()> {
    if record.version != 1 || record.snapshot.version != 1 {
        return Err(invalid("unsupported persisted budget version"));
    }
    for (count, limit) in [
        (
            record.snapshot.provider_requests,
            record.limits.provider_requests,
        ),
        (record.snapshot.tool_calls, record.limits.tool_calls),
        (
            record.snapshot.correction_passes,
            record.limits.correction_passes,
        ),
        (record.snapshot.retries, record.limits.retries),
    ] {
        if limit.is_some_and(|limit| count > limit) {
            return Err(invalid("persisted counter exceeds limit"));
        }
    }
    if record
        .limits
        .overall_ms
        .is_some_and(|limit| record.snapshot.started_at.checked_add(limit).is_none())
    {
        return Err(invalid("deadline overflow"));
    }
    Ok(())
}

fn encode(record: &PersistedBudget) -> HostResult<Value> {
    serde_json::to_value(record).map_err(|_| invalid("budget serialization failed"))
}

/// Restores cumulative consumption. Missing state after a prior attempt fails closed.
pub async fn restore_execution_budget(
    context: &ExecutionContext,
    limits: BudgetLimits,
    runtime: Arc<dyn Runtime>,
) -> HostResult<Arc<ExecutionBudget>> {
    let task = context
        .store
        .get_task(&context.task.task_id)
        .await?
        .ok_or_else(|| invalid("task missing"))?;
    let previous = task.runtime_state.clone();
    let record = if let Some(value) = previous.clone() {
        let mut record = decode(value)?;
        if serde_json::to_value(&record.limits).ok() != serde_json::to_value(&limits).ok() {
            return Err(invalid("execution limits changed across attempts"));
        }
        if record.attempt_number == 0 || record.attempt_number > task.attempt_count {
            return Err(invalid("invalid persisted attempt number"));
        }
        let retries = u64::from(task.attempt_count - record.attempt_number);
        record.snapshot.retries = record
            .snapshot
            .retries
            .checked_add(retries)
            .ok_or_else(|| invalid("retry counter overflow"))?;
        if record
            .limits
            .retries
            .is_some_and(|limit| record.snapshot.retries > limit)
        {
            return Err(HostError::Execution(
                "execution_budget_exhausted: retries".into(),
            ));
        }
        record.attempt_number = task.attempt_count;
        record
    } else {
        if task.attempt_count != 1 {
            return Err(invalid("missing durable budget after prior attempt"));
        }
        PersistedBudget {
            version: 1,
            attempt_number: task.attempt_count,
            limits,
            snapshot: BudgetSnapshot {
                version: 1,
                started_at: task
                    .enqueued_at
                    .try_into()
                    .map_err(|_| invalid("negative start time"))?,
                provider_requests: 0,
                tool_calls: 0,
                correction_passes: 0,
                retries: 0,
            },
        }
    };
    validate(&record)?;
    let encoded = encode(&record)?;
    context
        .store
        .checkpoint_runtime_state(
            context.fence.clone(),
            previous,
            encoded.clone(),
            context.runtime.now_ms(),
        )
        .await?;
    let checkpoint = Arc::new(DurableCheckpoint {
        context: context.clone(),
        previous: Mutex::new(encoded),
        limits: record.limits.clone(),
    });
    let budget = ExecutionBudget::new(record.limits, runtime, Some(record.snapshot))
        .map_err(|error| invalid(&error.to_string()))?
        .with_checkpoint(checkpoint);
    Ok(Arc::new(budget))
}

struct DurableCheckpoint {
    context: ExecutionContext,
    previous: Mutex<Value>,
    limits: BudgetLimits,
}

#[async_trait]
impl BudgetCheckpoint for DurableCheckpoint {
    async fn checkpoint(&self, mut snapshot: BudgetSnapshot) -> Result<(), CoreError> {
        let mut previous = self.previous.lock().await;
        let old = decode(previous.clone()).map_err(core_error)?;
        if snapshot.version != old.snapshot.version
            || snapshot.started_at != old.snapshot.started_at
        {
            return Err(CoreError::new("invalid_budget", "budget identity changed"));
        }
        snapshot.provider_requests = snapshot
            .provider_requests
            .max(old.snapshot.provider_requests);
        snapshot.tool_calls = snapshot.tool_calls.max(old.snapshot.tool_calls);
        snapshot.correction_passes = snapshot
            .correction_passes
            .max(old.snapshot.correction_passes);
        snapshot.retries = snapshot.retries.max(old.snapshot.retries);
        let record = PersistedBudget {
            version: 1,
            attempt_number: old.attempt_number,
            limits: self.limits.clone(),
            snapshot,
        };
        validate(&record).map_err(core_error)?;
        let state = encode(&record).map_err(core_error)?;
        if state == *previous {
            return Ok(());
        }
        self.context
            .store
            .checkpoint_runtime_state(
                self.context.fence.clone(),
                Some(previous.clone()),
                state.clone(),
                self.context.runtime.now_ms(),
            )
            .await
            .map_err(|_| {
                CoreError::new(
                    "budget_checkpoint_failed",
                    "execution budget checkpoint failed",
                )
            })?;
        *previous = state;
        Ok(())
    }
}

fn core_error(error: HostError) -> CoreError {
    CoreError::new("invalid_budget", &error.to_string())
}

#[cfg(test)]
mod tests;
