//! Chat-keyed operator resolution of uncertain execution receipts (ADR 0020),
//! shared by the session service and the host's static fast path.

use agentkit_core::ToolResult;
use agentkit_store::SqliteStore;
use serde_json::Value;

/// Wrap an operator-supplied completion in the model-facing `ToolResult`
/// shape, so replay and history reconciliation read it like any recorded
/// result (same contract as the session service's wrapper). A string becomes
/// the summary; any other JSON value becomes data.
fn operator_completed_result(result: Value) -> Result<Value, String> {
    let (summary, data) = match &result {
        Value::String(text) => (Some(text.clone()), result),
        other => (None, other.clone()),
    };
    serde_json::to_value(ToolResult {
        ok: true,
        data: Some(data),
        sources: vec![],
        warnings: vec![],
        truncated: false,
        limits: agentkit_core::ToolLimits::default(),
        model_data: None,
        summary,
        status: None,
        model_content: None,
    })
    .map_err(|error| format!("unserializable result: {error}"))
}

/// The static fast path's operator resolve route (ADR 0020).
///
/// A chat the host serves without a session (no contributors, no guards:
/// [`wrap_static_writes`] owns its write tools) has no park slot and no
/// session surface, so an uncertain receipt would otherwise be a permanent
/// dead end: every reissue meets the `tool_result_uncertain` refusal and
/// nothing can ever close the receipt. This is the explicit, auditable way
/// out — the same ownership, idempotency and active-run policy the session
/// service applies, keyed by the receipt's chat instead of a session, with
/// the decision (outcome, note, actor, time) written into the receipt
/// itself by the store.
///
/// It never replays anything: an unresolved uncertain effect stays refused
/// (the replay barrier above is the only gate that lets an effect through),
/// and this function neither executes nor re-admits the effect. It only
/// records what the operator verified — `completed` (the effect did happen;
/// the supplied result becomes its durable record), `not_started` (swept: it
/// never began), or `acknowledged` (reconciled: the outcome stands unknown,
/// the block is lifted). The `approvalGeneration` carried by the refusal
/// names the exact `execution_id` to resolve.
///
/// Errors: `not_found` when no receipt of that chat carries the id (another
/// chat's receipt is not this chat's to resolve); the store's
/// `receipt_run_active` while the run may still record its own result, and
/// `receipt_not_uncertain` for any finished receipt that is not the exact
/// idempotent repeat of an earlier decision; `invalid_input` for a
/// malformed result contract.
#[allow(clippy::too_many_arguments)] // the full audit record, passed once
pub async fn resolve_uncertain_execution(
    store: &SqliteStore,
    chat_id: &str,
    execution_id: &str,
    outcome: agentkit_store::ResolutionOutcome,
    result: Option<Value>,
    note: Option<String>,
    resolved_by: Option<String>,
    now: i64,
) -> crate::SessionsResult<agentkit_store::ExecutionReceiptRecord> {
    let invalid = |message: &str| crate::SessionsError::ReceiptInvalidInput(message.to_owned());
    let receipt = store
        .list_executions_by_chat(chat_id.to_owned())
        .await
        .map_err(crate::SessionsError::Store)?
        .into_iter()
        .find(|record| record.execution_id == execution_id)
        // Same answer for "no such receipt" and "another chat's receipt":
        // existence is not leaked across chats.
        .ok_or_else(|| crate::SessionsError::ReceiptNotFound {
            execution_id: execution_id.to_owned(),
            chat_id: chat_id.to_owned(),
        })?;
    let result = match (outcome, result) {
        (agentkit_store::ResolutionOutcome::Completed, Some(value)) => {
            Some(operator_completed_result(value).map_err(|error| invalid(&error))?)
        }
        (agentkit_store::ResolutionOutcome::Completed, None) => {
            return Err(invalid("A completed resolution requires a result."));
        }
        (_, Some(_)) => {
            return Err(invalid("Only a completed resolution may carry a result."));
        }
        (_, None) => None,
    };
    store
        .resolve_execution(
            receipt.execution_id,
            agentkit_store::ResolveExecution {
                outcome,
                result,
                note,
                resolved_by,
                now,
            },
        )
        .await
        .map_err(crate::SessionsError::Store)
}
