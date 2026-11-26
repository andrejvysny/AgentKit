//! The uncertainty probe of `before_guards`: which unresolved executions
//! block a call, and what the call may do about them.

use super::{DurableExecution, receipt_refusal, receipt_unreadable, uncertain_message};
use crate::{ApprovalGrant, PreGuard};
use agentkit_core::{CoreError, ToolContext, ToolDefinition, ToolResult};
use agentkit_store::{ExecutionReceiptRecord, ExecutionReceiptStatus};
use serde_json::json;

/// One earlier execution of this operation without a durable result.
pub(super) struct Blocking {
    generation: String,
    refusal: ToolResult,
    /// The evidence recorded no argument fingerprint.
    arguments_unknown: bool,
    /// Where the evidence lives, which decides when an operator can
    /// resolve it.
    source: Source,
}

/// Where an unresolved execution is recorded.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    /// A receipt of this run: the store refuses to resolve it while the run
    /// is active (`receipt_run_active`).
    ThisRun,
    /// A receipt of another run of this chat: resolvable once that run ended.
    OtherRun,
    /// A barrier: resolvable by its id at any time.
    Barrier,
}

impl DurableExecution {
    /// Steps 2-4 of the probe. This run and the other runs each contribute
    /// at most their newest unresolved execution; every matching barrier
    /// counts (`barrier_blocking`).
    pub(super) async fn blocking(
        &self,
        context: &ToolContext,
        tool: &ToolDefinition,
        receipts: &[ExecutionReceiptRecord],
        args: &str,
        ts_args: &str,
    ) -> Result<Vec<Blocking>, CoreError> {
        let same_operation = |record: &ExecutionReceiptRecord| {
            record.tool_name == tool.name
                && agentkit_store::args_match(
                    record.args_hash_provenance,
                    &record.args_sha256,
                    args,
                    Some(ts_args),
                )
        };
        let mut blocking = vec![];
        // 2. Within this run: the newest receipt of this operation, when it
        //    is admitted without a result.
        if let Some(previous) = receipts
            .iter()
            .rev()
            .find(|record| same_operation(record))
            .filter(|record| record.status == ExecutionReceiptStatus::Admitted)
        {
            blocking.push(Blocking {
                generation: previous.execution_id.clone(),
                refusal: receipt_refusal(
                    context,
                    "tool_result_uncertain",
                    &uncertain_message(),
                    Some(json!({"approvalGeneration": previous.execution_id})),
                ),
                arguments_unknown: false,
                source: Source::ThisRun,
            });
        }
        // 3. Cross-run uncertainty guard (same chat): an uncertain mutation
        //    under any run of this chat blocks an identical reissue even
        //    under a fresh tool-call id. An identical completed operation
        //    is not blocked.
        if let Some(previous) = self
            .context
            .store
            .list_executions_by_chat(self.chat_id.clone())
            .await
            .map_err(receipt_unreadable)?
            .iter()
            .rev()
            .filter(|record| record.run_id != self.context.task.task_id)
            .find(|record| {
                same_operation(record) && record.status == ExecutionReceiptStatus::Admitted
            })
        {
            blocking.push(Blocking {
                generation: previous.execution_id.clone(),
                refusal: receipt_refusal(
                    context,
                    "tool_result_uncertain",
                    &format!(
                        "{}; the pending execution belongs to run {}",
                        uncertain_message(),
                        previous.run_id
                    ),
                    Some(
                        json!({"approvalGeneration": previous.execution_id, "pendingRunId": previous.run_id}),
                    ),
                ),
                arguments_unknown: false,
                source: Source::OtherRun,
            });
        }
        blocking.extend(self.barrier_blocking(context, tool, args, ts_args).await?);
        Ok(blocking)
    }

    /// Step 4, durable uncertainty barriers: admitted effects whose receipt
    /// could not be placed (the tolerant TypeScript import records one
    /// instead of silently omitting the row) block the identical reissue
    /// too, keyed by tool name plus best-known identity. A barrier with no
    /// argument fingerprint blocks the tool by name: the earlier call's
    /// arguments are unknown. Every matching barrier counts, newest first:
    /// the store's admission gate holds each one until an answer names it,
    /// so a card about one of two would get a yes that runs nothing.
    async fn barrier_blocking(
        &self,
        context: &ToolContext,
        tool: &ToolDefinition,
        args: &str,
        ts_args: &str,
    ) -> Result<Vec<Blocking>, CoreError> {
        let barriers = self
            .context
            .store
            .list_uncertainty_barriers(self.chat_id.clone(), self.context.task.task_id.clone())
            .await
            .map_err(receipt_unreadable)?;
        Ok(barriers
            .iter()
            .rev()
            .filter(|barrier| barrier.blocks(&tool.name, args, Some(ts_args)))
            .map(|barrier| Blocking {
                generation: barrier.barrier_id.clone(),
                refusal: receipt_refusal(
                    context,
                    "tool_result_uncertain",
                    &format!(
                        "{}; the unresolved evidence is a barrier: {}",
                        uncertain_message(),
                        barrier.reason
                    ),
                    Some(json!({"approvalGeneration": barrier.barrier_id})),
                ),
                arguments_unknown: barrier.args_sha256.is_none(),
                source: Source::Barrier,
            })
            .collect())
    }
}

/// An answer reauthorizes exactly the unresolved generation it named, and
/// only when its card led with that uncertainty: a card that carried the
/// generation without showing it (the release before uncertainty cards)
/// opens nothing, and the call asks again.
fn covers(grant: Option<&ApprovalGrant>, generation: &str) -> bool {
    grant.is_some_and(|grant| {
        grant.uncertainty_card && grant.uncertainty_generation.as_deref() == Some(generation)
    })
}

/// What the call may do. One unresolved execution: it proceeds when its
/// answer covers it, else it asks about it. Several: one answer names one
/// generation, so no answer can open the call; it ends with the uncertain
/// refusal instead of asking a question whose yes runs nothing (a re-park
/// on an answered request would wait forever). An operator resolves the
/// executions the refusal names.
pub(super) fn decide(
    context: &ToolContext,
    mut blocking: Vec<Blocking>,
    grant: Option<&ApprovalGrant>,
) -> PreGuard {
    if blocking.len() > 1 {
        return PreGuard::Refused(several(context, blocking));
    }
    let Some(only) = blocking.pop() else {
        return PreGuard::Proceed;
    };
    if covers(grant, &only.generation) {
        PreGuard::Reauthorized {
            generation: only.generation,
        }
    } else {
        PreGuard::Uncertain {
            refusal: only.refusal,
            generation: only.generation,
            arguments_unknown: only.arguments_unknown,
        }
    }
}

/// The refusal of a call that several unresolved executions block. The
/// ones an operator can resolve now come first (`approvalGeneration` names
/// the first); this run's own receipt comes last and the message says it
/// can be resolved only after the run ends.
fn several(context: &ToolContext, mut blocking: Vec<Blocking>) -> ToolResult {
    blocking.sort_by_key(|item| item.source == Source::ThisRun);
    let generations: Vec<&str> = blocking
        .iter()
        .map(|item| item.generation.as_str())
        .collect();
    let this_run = blocking
        .iter()
        .find(|item| item.source == Source::ThisRun)
        .map(|item| {
            format!(
                "; {} belongs to this run and can be resolved only after it ends",
                item.generation
            )
        })
        .unwrap_or_default();
    receipt_refusal(
        context,
        "tool_result_uncertain",
        &format!(
            "{}; {} earlier executions of this operation are unresolved and one approval can reauthorize only one of them, so an operator must resolve them first, starting with {}{this_run}",
            uncertain_message(),
            generations.len(),
            generations[0],
        ),
        Some(json!({
            "approvalGeneration": generations[0],
            "unresolvedGenerations": generations,
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentkit_core::{CancellationToken, ToolLimits};

    fn context() -> ToolContext {
        ToolContext {
            run_id: "run".into(),
            tool_call_id: Some("call".into()),
            attempt_id: None,
            chat_id: Some("chat".into()),
            scope_id: None,
            user_id: None,
            bindings: vec![],
            limits: ToolLimits::default(),
            cancellation: CancellationToken::new(),
            dispatch_guard: None,
            metadata: Default::default(),
            suspension: None,
        }
    }

    fn blocking(generation: &str, source: Source) -> Blocking {
        Blocking {
            generation: generation.into(),
            refusal: receipt_refusal(&context(), "tool_result_uncertain", "x", None),
            arguments_unknown: false,
            source,
        }
    }

    #[test]
    fn several_unresolved_name_the_resolvable_ones_first() {
        let all = vec![
            blocking("own", Source::ThisRun),
            blocking("other", Source::OtherRun),
            blocking("barrier:b", Source::Barrier),
        ];
        let PreGuard::Refused(refusal) = decide(&context(), all, None) else {
            panic!("several unresolved executions never ask");
        };
        let data = refusal.data.unwrap();
        assert_eq!(data["approvalGeneration"], "other");
        assert_eq!(
            data["unresolvedGenerations"],
            json!(["other", "barrier:b", "own"])
        );
        let message = data["errorMessage"].as_str().unwrap_or_default();
        assert!(
            message.contains("own belongs to this run and can be resolved only after it ends"),
            "{data}"
        );
        let two = vec![
            blocking("own", Source::ThisRun),
            blocking("barrier:b", Source::Barrier),
        ];
        let PreGuard::Refused(refusal) = decide(&context(), two, None) else {
            panic!("several unresolved executions never ask");
        };
        assert_eq!(refusal.data.unwrap()["approvalGeneration"], "barrier:b");
    }
}
