//! Durable execution ownership (ADR 0020), ported from the TypeScript
//! reference (`turn/guarded-tool.ts` + `turn/durable-execution.ts`).
//!
//! A mutating tool body may only run after a *newly successful* atomic
//! admission; the result is persisted before it is delivered. An effect whose
//! completion is unknown (admitted, no durable result) is never silently
//! re-executed: replays restore the recorded result, uncertain operations are
//! refused, and unreadable recovery evidence blocks execution instead of
//! reading as an empty uncertainty set.
//!
//! Receipts bind: the logical operation (run + tool call), the execution
//! attempt, canonical argument and execution-context fingerprints, and the
//! (dormant until the approval flow lands) authorization fields.

use super::staging::{ToolContributionContext, ToolGuardContext};
use crate::ExecutionContext;
use agentkit_core::{CoreError, ToolContext, ToolDefinition, ToolResult};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::Arc;

/// Host-owned durable execution bundle for one staged run.
pub struct DurableExecution {
    pub context: ExecutionContext,
    pub chat_id: String,
    /// Tools the host additionally treats as mutating beyond `effect: write`
    /// (TS `durable.isMutating`).
    pub is_mutating: Option<Arc<MutatingPolicy>>,
    /// Answered approvals this attempt resumes (tool call id -> the grant,
    /// ADR 0019 §2). An approved call spends its single-use approval at
    /// admission and is exempt from the cross-run uncertainty refusal that
    /// an unapproved identical reissue would hit.
    pub approvals: Arc<std::collections::HashMap<String, ApprovalGrant>>,
    /// Exact-call remembering policy for approvable refusals (ADR 0019
    /// part 2). Absent: every approvable call asks each time.
    pub standing: Option<Arc<dyn StandingApprovalPolicy>>,
}

/// An answered approval bound to one exact call: the answered request id
/// (spent at admission, single-use via the store's UNIQUE index) and the
/// canonical arguments hash it was granted for (a changed call is not this
/// grant's to spend). `uncertainty_generation` is the execution id of the
/// unresolved admitted receipt an answer explicitly reauthorized: the
/// store's pending-generation gate only opens for exactly that value, so
/// the grant cannot spend itself on a different (later) uncertainty.
#[derive(Clone, Debug)]
pub struct ApprovalGrant {
    pub request_id: String,
    pub args_sha256: String,
    pub uncertainty_generation: Option<String>,
}

/// Which approvable refusals may be remembered, and under what digest. The
/// coding pack implements this for its shell tools: a policy that declines
/// to key a call (high-risk, plan, malformed) forces a fresh ask.
#[async_trait::async_trait]
pub trait StandingApprovalPolicy: Send + Sync {
    /// `(digest, label)` for this exact call, or `None` when uncoverable.
    /// `reasons` are every approvable refusal the guards gave; a policy
    /// covers only the ordinary `approval_required:` surface.
    async fn key_for(
        &self,
        context: &super::staging::ToolGuardContext,
        reasons: &[String],
    ) -> crate::HostResult<Option<(String, String)>>;
}

/// TS `durable.isMutating` predicate.
pub type MutatingPolicy = dyn Fn(&str) -> bool + Send + Sync;

impl std::fmt::Debug for DurableExecution {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DurableExecution")
            .field("run_id", &self.context.task.task_id)
            .field("chat_id", &self.chat_id)
            .finish_non_exhaustive()
    }
}

pub(super) fn sha256_hex(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
}

/// Canonical JSON: object keys sorted, exactly as the TypeScript
/// `argsSha256Of` (canonical JSON with sorted keys → sha-256). Rebuilt into a
/// BTreeMap-backed tree so insertion order can never leak into the hash.
fn canonical_sha256(value: &Value) -> String {
    let canonical =
        serde_json::to_string(&sort_into_btree(value)).unwrap_or_else(|_| "null".into());
    sha256_hex(&canonical)
}

fn sort_into_btree(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let sorted: std::collections::BTreeMap<String, Value> = map
                .iter()
                .map(|(k, v)| (k.clone(), sort_into_btree(v)))
                .collect();
            Value::Object(sorted.into_iter().collect())
        }
        Value::Array(items) => Value::Array(items.iter().map(sort_into_btree).collect()),
        other => other.clone(),
    }
}

/// sha-256 of the canonical-JSON arguments.
pub fn args_sha256(input: &Value) -> String {
    canonical_sha256(input)
}

/// The host execution-context binding: everything that must stay identical
/// for a recorded result to be a faithful replay of the same operation.
/// `effect_context` carries the tool's real execution environment
/// (workspace, sandbox/network policy, runner selection); an absent value
/// is the empty object, never a stand-in for an unbound environment.
pub fn execution_context_sha256(
    durable: &DurableExecution,
    contribution: &ToolContributionContext,
    tool: &ToolDefinition,
    effect_context: Option<&Value>,
) -> String {
    canonical_sha256(&json!({
        "chatId": durable.chat_id,
        "scopeId": contribution.scope_id,
        "bindings": contribution.bindings,
        "toolName": tool.name,
        "toolVersion": tool.version,
        "context": effect_context.unwrap_or(&Value::Object(Default::default())),
    }))
}

/// Deterministic receipt identity per (run, call, approval).
pub fn execution_id(run_id: &str, tool_call_id: &str, approval_request_id: Option<&str>) -> String {
    canonical_sha256(&json!({
        "runId": run_id,
        "toolCallId": tool_call_id,
        "approvalRequestId": approval_request_id.unwrap_or("initial"),
    }))
}

fn receipt_refusal(
    context: &ToolContext,
    code: &str,
    reason: &str,
    extra: Option<Value>,
) -> ToolResult {
    let mut data =
        json!({"errorCode": code, "errorMessage": reason, "phase": "receipts", "retryable": false});
    if let Some(Value::Object(extra)) = extra {
        for (key, value) in extra {
            data[key] = value;
        }
    }
    ToolResult {
        ok: false,
        data: Some(data.clone()),
        model_data: Some(data),
        summary: Some(reason.into()),
        sources: vec![],
        warnings: vec![],
        truncated: false,
        limits: context.limits.clone(),
        status: None,
    }
}

fn uncertain_message() -> String {
    "a previous execution of this operation was admitted without a durable result; fresh approval is required".into()
}

pub(super) enum PreGuard {
    Proceed,
    Replay(ToolResult),
    /// The operation is uncertain (admitted without a durable result) and
    /// no answer covers this generation. The wrapper folds it into the
    /// approval flow: the human is asked, the ask carries the exact
    /// unresolved generation, and a refusal is the fallback when this host
    /// cannot park.
    Uncertain {
        refusal: ToolResult,
        generation: String,
    },
}

#[allow(clippy::large_enum_variant)] // refusals carry the full model-facing envelope
pub(super) enum Admission {
    /// Non-mutating tool: no receipt required.
    Skipped,
    Granted {
        execution_id: String,
    },
    Refused(ToolResult),
}

impl DurableExecution {
    fn needs_receipt(&self, tool: &ToolDefinition) -> bool {
        tool.effect == "write"
            || self
                .is_mutating
                .as_ref()
                .is_some_and(|policy| policy(&tool.name))
    }

    /// Replay barrier + uncertainty probe. Runs before the guard chain so a
    /// recorded completion is served even when a guard would now refuse, and
    /// an uncertain operation is refused even when guards would allow it.
    /// Non-mutating tools never have receipts, so they skip the scan.
    pub(super) async fn before_guards(
        &self,
        context: &ToolContext,
        contribution: &ToolGuardContext,
        tool: &ToolDefinition,
        effect_context: Option<&Value>,
        input: &Value,
    ) -> Result<PreGuard, CoreError> {
        if !self.needs_receipt(tool) {
            return Ok(PreGuard::Proceed);
        }
        let Some(tool_call_id) = context.tool_call_id.clone() else {
            return Err(CoreError::new(
                "invalid_execution",
                "Mutating execution requires a stable tool-call identity",
            ));
        };
        let receipts = self
            .context
            .store
            .list_executions(self.context.task.task_id.clone())
            .await
            .map_err(receipt_unreadable)?;
        // An answer only reauthorizes the exact unresolved generation it
        // named: grant.uncertainty_generation must equal the pending
        // receipt's execution id. An unrelated (or stale) grant never
        // opens an uncertain effect.
        let covers = |previous: &agentkit_store::ExecutionReceiptRecord| {
            self.approvals.get(&tool_call_id).is_some_and(|grant| {
                grant.uncertainty_generation.as_deref() == Some(previous.execution_id.as_str())
            })
        };
        // 1. Replay barrier: the last receipt for this exact tool call.
        //    (Only the newest receipt counts: a newest-admitted call must
        //    read as uncertain even if an older attempt completed.)
        if let Some(last) = receipts
            .iter()
            .rev()
            .find(|r| r.tool_call_id == tool_call_id)
            && last.status == agentkit_store::ExecutionReceiptStatus::Completed
        {
            let args = args_sha256(input);
            let context_sha =
                execution_context_sha256(self, &contribution.contribution, tool, effect_context);
            if last.tool_name != tool.name
                || last.args_sha256 != args
                || last.context_sha256 != context_sha
            {
                return Err(CoreError::new(
                    "execution_context_mismatch",
                    "The recovered execution no longer matches its recorded operation or context.",
                ));
            }
            let result = last.result.clone().ok_or_else(|| {
                CoreError::new(
                    "recovery_history_unreadable",
                    "A completed execution receipt carries no result; mutations remain blocked.",
                )
            })?;
            let stored: ToolResult = serde_json::from_value(result).map_err(|_| {
                CoreError::new(
                    "recovery_history_unreadable",
                    "A completed execution receipt is malformed; mutations remain blocked.",
                )
            })?;
            return Ok(PreGuard::Replay(stored));
        }
        // 2. Uncertainty probe within this run: the human must explicitly
        //    reauthorize the unresolved generation (or the call proceeds
        //    when an answered grant already covers it).
        let args = args_sha256(input);
        if let Some(previous) = receipts
            .iter()
            .rev()
            .find(|r| r.tool_name == tool.name && r.args_sha256 == args)
            .filter(|r| r.status == agentkit_store::ExecutionReceiptStatus::Admitted)
            && !covers(previous)
        {
            return Ok(PreGuard::Uncertain {
                refusal: receipt_refusal(
                    context,
                    "tool_result_uncertain",
                    &uncertain_message(),
                    Some(json!({"approvalGeneration": previous.execution_id})),
                ),
                generation: previous.execution_id.clone(),
            });
        }
        // 3. Cross-run uncertainty guard (same chat): an uncertain mutation
        // under any run of this chat blocks an identical reissue even under a
        // fresh tool-call id. An identical completed operation is not blocked;
        // neither is a reissue whose answer names exactly this pending
        // generation.
        if let Some(previous) = self
            .context
            .store
            .list_executions_by_chat(self.chat_id.clone())
            .await
            .map_err(receipt_unreadable)?
            .iter()
            .rev()
            .filter(|r| r.run_id != self.context.task.task_id)
            .find(|r| {
                r.tool_name == tool.name
                    && r.args_sha256 == args
                    && r.status == agentkit_store::ExecutionReceiptStatus::Admitted
            })
            && !covers(previous)
        {
            return Ok(PreGuard::Uncertain {
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
                generation: previous.execution_id.clone(),
            });
        }
        Ok(PreGuard::Proceed)
    }

    /// Atomic admission after the guard chain, before the body. Only a newly
    /// admitted receipt proceeds; the store transaction has already ended.
    pub(super) async fn admit(
        &self,
        context: &ToolContext,
        contribution: &ToolGuardContext,
        tool: &ToolDefinition,
        effect_context: Option<&Value>,
        input: &Value,
    ) -> Result<Admission, CoreError> {
        if !self.needs_receipt(tool) {
            return Ok(Admission::Skipped);
        }
        let tool_call_id = context.tool_call_id.clone().ok_or_else(|| {
            CoreError::new(
                "invalid_execution",
                "Mutating execution requires a stable tool-call identity",
            )
        })?;
        // A resumed approved call binds to its answered request: the
        // execution identity differs from the initial attempt, and the
        // store's UNIQUE approval index makes the approval single-use.
        // The pending-generation gate opens only for the generation the
        // human actually named — never for the request id.
        let grant = self.approvals.get(&tool_call_id);
        let approval_request_id = grant.map(|grant| grant.request_id.clone());
        let execution = execution_id(
            &self.context.task.task_id,
            &tool_call_id,
            approval_request_id.as_deref(),
        );
        let admission = agentkit_store::ExecutionReceiptAdmission {
            execution_id: execution.clone(),
            run_id: self.context.task.task_id.clone(),
            chat_id: self.chat_id.clone(),
            tool_call_id,
            tool_name: tool.name.clone(),
            args_sha256: args_sha256(input),
            context_sha256: execution_context_sha256(
                self,
                &contribution.contribution,
                tool,
                effect_context,
            ),
            attempt_id: self.context.attempt_id.clone(),
            approval_request_id: approval_request_id.clone(),
            approval_generation: grant.and_then(|grant| grant.uncertainty_generation.clone()),
            now: self.context.runtime.now_ms(),
        };
        let admitted = self
            .context
            .store
            .admit_execution(self.context.fence.clone(), admission)
            .await
            .map_err(receipt_unreadable)?;
        if !admitted {
            return Ok(Admission::Refused(receipt_refusal(
                context,
                "execution_already_admitted",
                "this authorization has already been consumed; its outcome must be recovered before another execution.",
                Some(json!({"executionId": execution})),
            )));
        }
        Ok(Admission::Granted {
            execution_id: execution,
        })
    }

    /// Persist the result before delivery. A failure here fails the run
    /// closed: an executed effect whose result could not be recorded must
    /// never be forgotten, and the receipt stays `admitted` — uncertain.
    pub(super) async fn complete(
        &self,
        execution_id: &str,
        result: &ToolResult,
    ) -> Result<(), CoreError> {
        let serialized = serde_json::to_value(result).map_err(|_| {
            CoreError::new(
                "result_unserializable",
                "The executed tool result cannot be recorded; the effect is uncertain.",
            )
        })?;
        self.context
            .store
            .complete_execution(
                self.context.fence.clone(),
                execution_id.to_owned(),
                serialized,
                self.context.runtime.now_ms(),
            )
            .await
            .map_err(|error| {
                CoreError::new(
                    "receipt_unavailable",
                    &format!("Durable execution result could not be persisted: {error}"),
                )
            })
    }
}

fn receipt_unreadable(error: agentkit_store::StoreError) -> CoreError {
    CoreError::new(
        "recovery_history_unreadable",
        &format!("Cannot read durable execution receipts; mutations remain blocked. ({error})"),
    )
}

/// Receipt-only wrapper for static write tools in the fast staging path.
/// It preserves the original definition and adds no guards: durable
/// admission, replay, and uncertainty govern the body exactly as in
/// `GuardedTool`, so the chat fast path is never a receipt bypass.
pub(crate) struct ReceiptedTool {
    tool: Arc<dyn agentkit_core::Tool>,
    durable: Arc<DurableExecution>,
    contribution: ToolContributionContext,
}

#[async_trait::async_trait]
impl agentkit_core::Tool for ReceiptedTool {
    fn definition(&self) -> &ToolDefinition {
        self.tool.definition()
    }
    async fn execute(&self, context: ToolContext, input: Value) -> Result<ToolResult, CoreError> {
        let definition = self.tool.definition();
        let effect_context = self.tool.effect_context();
        let snapshot = ToolGuardContext {
            contribution: self.contribution.clone(),
            namespace: "static".into(),
            tool: definition.clone(),
            arguments: Some(input.clone()),
        };
        match self
            .durable
            .before_guards(
                &context,
                &snapshot,
                definition,
                effect_context.as_ref(),
                &input,
            )
            .await?
        {
            PreGuard::Replay(result) => return Ok(result),
            // The static fast path has no guard chain and no park slot:
            // the uncertain refusal stands as the final answer, exactly as
            // before — only the session wrapper can ask a human.
            PreGuard::Uncertain { refusal, .. } => return Ok(refusal),
            PreGuard::Proceed => {}
        }
        if let Some(guard) = &context.dispatch_guard {
            guard.check().await?;
        }
        let execution_id = match self
            .durable
            .admit(
                &context,
                &snapshot,
                definition,
                effect_context.as_ref(),
                &input,
            )
            .await?
        {
            Admission::Skipped => None,
            Admission::Granted { execution_id } => Some(execution_id),
            Admission::Refused(result) => return Ok(result),
        };
        if context.cancellation.is_cancelled() {
            return Err(CoreError::new("cancelled", "aborted"));
        }
        let result = self.tool.execute(context, input).await;
        if let (Some(id), Ok(tool_result)) = (execution_id.as_deref(), &result) {
            self.durable.complete(id, tool_result).await?;
        }
        result
    }
}

/// Wrap the write-effect tools of a static registry in [`ReceiptedTool`],
/// leaving read tools (and definitions, ordering, and identity) untouched.
pub(crate) fn wrap_static_writes(
    registry: &agentkit_core::ToolRegistry,
    durable: Arc<DurableExecution>,
    contribution: &ToolContributionContext,
) -> agentkit_core::ToolRegistry {
    let mut wrapped = agentkit_core::ToolRegistry::new();
    for definition in registry.definitions() {
        let Some(tool) = registry.get(&definition.name) else {
            continue;
        };
        let tool: Arc<dyn agentkit_core::Tool> = if definition.effect == "write" {
            Arc::new(ReceiptedTool {
                tool,
                durable: durable.clone(),
                contribution: contribution.clone(),
            })
        } else {
            tool
        };
        // The source registry was already collision-checked; registration
        // cannot fail here, but a duplicate would be a staging bug worth
        // surfacing loudly.
        wrapped
            .register(tool)
            .expect("static registry names are unique");
    }
    wrapped
}
