//! Operator-facing view over uncertain execution receipts (ADR 0020).
//!
//! A receipt left `admitted` without a result blocks every identical reissue
//! until a human says what really happened. Resolution is therefore an
//! explicit session-service call, never a side effect of anything else, and
//! it is audited inside the receipt itself.

use super::AgentSessionService;
use crate::{SessionsError, SessionsResult};
use agentkit_core::{ToolLimits, ToolResult};
use agentkit_store::{
    ExecutionReceiptRecord, ExecutionReceiptStatus, ResolutionOutcome, ResolveExecution, StoreError,
};
use serde_json::Value;

/// Wrap an operator-supplied completion in the model-facing `ToolResult`
/// shape, so replay and history reconciliation read it like any recorded
/// result. A string becomes the summary; any other JSON value becomes data.
fn operator_tool_result(result: Value) -> Result<Value, SessionsError> {
    let (summary, data) = match result {
        Value::String(text) => (Some(text.clone()), Value::String(text)),
        other => (None, other),
    };
    serde_json::to_value(ToolResult {
        ok: true,
        data: Some(data),
        sources: vec![],
        warnings: vec![],
        truncated: false,
        limits: ToolLimits::default(),
        model_data: None,
        summary,
        status: None,
        model_content: None,
    })
    .map_err(|error| SessionsError::invalid_input(format!("unserializable result: {error}")))
}

fn map_store_error(error: StoreError) -> SessionsError {
    SessionsError::ReceiptStore(crate::ReceiptStoreError(error))
}

impl AgentSessionService {
    /// Uncertain receipts (`admitted`, no result) of the session's chat,
    /// oldest first.
    pub async fn list_uncertain_executions(
        &self,
        session_id: &str,
    ) -> SessionsResult<Vec<ExecutionReceiptRecord>> {
        self.require_session(session_id).await?;
        let receipts = self
            .store()
            .list_executions_by_chat(session_id.to_owned())
            .await?;
        Ok(receipts
            .into_iter()
            .filter(|receipt| receipt.status == ExecutionReceiptStatus::Admitted)
            .collect())
    }

    /// Explicitly close one uncertain receipt. `completed` records the
    /// operator-supplied `result`; `not_started` / `acknowledged` record no
    /// result and merely stop the receipt blocking identical calls. The
    /// receipt must belong to this session and its run must be finished.
    pub async fn resolve_uncertain_execution(
        &self,
        session_id: &str,
        execution_id: &str,
        outcome: ResolutionOutcome,
        result: Option<Value>,
        note: Option<String>,
        resolved_by: Option<String>,
    ) -> SessionsResult<ExecutionReceiptRecord> {
        self.require_session(session_id).await?;
        let receipt = self
            .store()
            .list_executions_by_chat(session_id.to_owned())
            .await?
            .into_iter()
            .find(|receipt| receipt.execution_id == execution_id)
            // Same answer for "no such receipt" and "another session's
            // receipt": existence is not leaked across sessions.
            .ok_or_else(|| {
                SessionsError::ExecutionNotFound(format!(
                    "Execution {execution_id} not found in {session_id}."
                ))
            })?;
        let result = match (outcome, result) {
            (ResolutionOutcome::Completed, Some(value)) => Some(operator_tool_result(value)?),
            (ResolutionOutcome::Completed, None) => {
                return Err(SessionsError::invalid_input(
                    "A completed resolution requires a result.",
                ));
            }
            (_, Some(_)) => {
                return Err(SessionsError::invalid_input(
                    "Only a completed resolution may carry a result.",
                ));
            }
            (_, None) => None,
        };
        self.store()
            .resolve_execution(
                receipt.execution_id,
                ResolveExecution {
                    outcome,
                    result,
                    note,
                    resolved_by,
                    now: self.now_ms(),
                },
            )
            .await
            .map_err(map_store_error)
    }

    /// Chat-keyed twin of [`Self::resolve_uncertain_execution`] for the
    /// static fast path (AGENTKIT-73): a chat the host serves without a
    /// session has no park slot, so `require_session` can never be satisfied
    /// and the session surface above is a permanent dead end for its
    /// receipts. Same decision record, same result rules — delegated to the
    /// governance route, which keys ownership on the receipt's chat and never
    /// executes, re-admits, or replays anything.
    pub async fn resolve_uncertain_execution_for_chat(
        &self,
        chat_id: &str,
        execution_id: &str,
        outcome: ResolutionOutcome,
        result: Option<Value>,
        note: Option<String>,
        resolved_by: Option<String>,
    ) -> SessionsResult<ExecutionReceiptRecord> {
        crate::receipts::resolve_uncertain_execution(
            self.store(),
            chat_id,
            execution_id,
            outcome,
            result,
            note,
            resolved_by,
            self.now_ms(),
        )
        .await
        .map_err(|error| match error {
            SessionsError::Store(error) => map_store_error(error),
            other => other,
        })
    }
}

#[cfg(test)]
mod receipt_error_tests;
