use super::GovernedProposalTool;
use crate::{HostError, HostResult};
use agentkit_core::{
    CoreError, Tool, ToolContext, ToolDefinition, ToolLimits, ToolRegistry, ToolResult,
};
use async_trait::async_trait;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct ToolContributionContext {
    pub chat_id: Option<String>,
    pub run_id: String,
    pub scope_id: Option<String>,
    pub actor: Value,
    pub actor_id: Option<String>,
    pub bindings: Vec<Value>,
    pub limits: ToolLimits,
    pub cancellation: CancellationToken,
    pub has_primary_binding: bool,
    pub timeout_ms: u64,
    /// Host-owned durable execution bundle (ADR 0020). When present, guarded
    /// mutating tools admit/complete receipts around their body; when absent,
    /// `effect: write` tools are refused rather than run unguarded.
    pub durable: Option<Arc<super::DurableExecution>>,
}

#[derive(Clone)]
pub enum ContributedTool {
    Read(Arc<dyn Tool>),
    /// Trusted hosts retain direct effects; guards authorize them and the domain
    /// owns durable receipts and reconciliation of uncertain external effects.
    DirectWrite(Arc<dyn Tool>),
    Write(Arc<GovernedProposalTool>),
}

impl ContributedTool {
    fn checked(&self, context: &ToolContributionContext) -> HostResult<Arc<dyn Tool>> {
        match self {
            Self::Read(tool) if tool.definition().effect == "read" => Ok(tool.clone()),
            Self::DirectWrite(tool) if tool.definition().effect == "write" => {
                let mut definition = tool.definition().clone();
                definition.description = format!(
                    "Direct write tool; host authorization and domain receipts are required. {}",
                    definition.description
                );
                Ok(Arc::new(DirectWriteTool {
                    inner: tool.clone(),
                    definition,
                }))
            }
            Self::Write(tool)
                if tool.matches_actor(&context.actor, context.actor_id.as_deref()) =>
            {
                Ok(tool.clone())
            }
            _ => Err(HostError::Execution("tool_effect_or_actor_mismatch".into())),
        }
    }
}

#[async_trait]
pub trait ToolContributor: Send + Sync {
    fn namespace(&self) -> &str;
    fn privileged(&self) -> bool {
        false
    }
    fn unbound_tool_names(&self) -> Option<BTreeSet<String>> {
        None
    }
    async fn contribute(
        &self,
        context: &ToolContributionContext,
    ) -> HostResult<Vec<ContributedTool>>;
}

/// Legacy tools enter the same guards. Proposal-only hosts opt into pruning direct writes.
pub struct StaticToolContributor {
    registry: Arc<ToolRegistry>,
    allow_direct_writes: bool,
}

impl StaticToolContributor {
    pub fn new(registry: Arc<ToolRegistry>) -> Self {
        Self {
            registry,
            allow_direct_writes: true,
        }
    }
    pub fn proposal_only(registry: Arc<ToolRegistry>) -> Self {
        Self {
            registry,
            allow_direct_writes: false,
        }
    }
    pub fn skipped_write_names(&self) -> Vec<String> {
        self.registry
            .definitions()
            .into_iter()
            .filter(|definition| !self.allow_direct_writes && definition.effect == "write")
            .map(|definition| definition.name)
            .collect()
    }
}

#[async_trait]
impl ToolContributor for StaticToolContributor {
    fn namespace(&self) -> &str {
        "static"
    }
    async fn contribute(
        &self,
        _context: &ToolContributionContext,
    ) -> HostResult<Vec<ContributedTool>> {
        Ok(self
            .registry
            .definitions()
            .into_iter()
            .filter_map(|definition| {
                let tool = self.registry.get(&definition.name)?;
                match definition.effect.as_str() {
                    "read" => Some(ContributedTool::Read(tool)),
                    "write" if self.allow_direct_writes => Some(ContributedTool::DirectWrite(tool)),
                    _ => None,
                }
            })
            .collect())
    }
}

struct DirectWriteTool {
    inner: Arc<dyn Tool>,
    definition: ToolDefinition,
}

#[async_trait]
impl Tool for DirectWriteTool {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }
    async fn execute(&self, context: ToolContext, input: Value) -> Result<ToolResult, CoreError> {
        self.inner.execute(context, input).await
    }
}

#[derive(Clone)]
pub struct ToolGuardContext {
    pub contribution: ToolContributionContext,
    pub namespace: String,
    pub tool: ToolDefinition,
    /// The live call's arguments, present on `canExecute` only (TS
    /// `ToolGuardContext.arguments?`; visibility never sees them). Guards
    /// that classify command text depend on this field.
    pub arguments: Option<Value>,
}

pub enum ToolGuardVerdict {
    Allowed,
    Refused(String),
    /// The call could run IF a human approves it (ADR 0016 §2, ADR 0019).
    /// The wrapper — not the guard — assembles the durable request: it owns
    /// the argument hash, the one-use approvals, and the standing grants.
    /// A `Refused` anywhere in the chain wins over any number of these
    /// (denial is final; approval cannot reopen a role ceiling).
    AwaitingHuman(ApprovalAsk),
}

/// The approvable refusal a guard offers: the model-facing reason, the text
/// the human approves on, and the mode a plan REQUESTS (recorded for the
/// human, never obeyed — the model may not author its own permissions).
/// The durable wrapper adds `uncertainty_generation` (the execution id of
/// the unresolved admitted receipt) so an approval is bound to the exact
/// uncertain effect it authorizes; a later uncertainty needs a new answer.
#[derive(Clone, Debug, Default)]
pub struct ApprovalAsk {
    pub reason: String,
    pub preview: Option<String>,
    pub requested_mode: Option<String>,
    pub uncertainty_generation: Option<String>,
}

#[async_trait]
pub trait ToolGuard: Send + Sync {
    async fn is_visible(&self, _context: &ToolGuardContext) -> HostResult<bool> {
        Ok(true)
    }
    async fn can_execute(&self, _context: &ToolGuardContext) -> HostResult<ToolGuardVerdict> {
        Ok(ToolGuardVerdict::Allowed)
    }
}

pub struct ContributorFailure {
    pub namespace: String,
    pub reason: String,
    pub timed_out: bool,
}

pub struct StagedTools {
    pub registry: Arc<ToolRegistry>,
    pub namespaces: BTreeMap<String, String>,
    pub failed: Vec<ContributorFailure>,
    pub skipped: Vec<String>,
}

pub async fn stage_tools(
    contributors: &[Arc<dyn ToolContributor>],
    guards: &[Arc<dyn ToolGuard>],
    context: ToolContributionContext,
) -> HostResult<StagedTools> {
    for contributor in contributors {
        validate_namespace(contributor.as_ref())?;
    }
    let mut failed = vec![];
    let mut owners = BTreeMap::new();
    let mut collected = vec![];
    for (index, contributor) in contributors.iter().enumerate() {
        let tools = contribute(contributor.as_ref(), &context, &mut failed).await;
        for tool in tools {
            let tool = tool.checked(&context)?;
            let name = &tool.definition().name;
            if owners.get(name).is_some_and(|owner| *owner != index) {
                return Err(HostError::Execution("tool_name_collision".into()));
            }
            owners.insert(name.clone(), index);
            collected.push((contributor.namespace().to_owned(), tool));
        }
    }
    let allowed = unbound_names(contributors, context.has_primary_binding);
    register_tools(collected, guards, &context, allowed, failed).await
}

async fn contribute(
    contributor: &dyn ToolContributor,
    context: &ToolContributionContext,
    failures: &mut Vec<ContributorFailure>,
) -> Vec<ContributedTool> {
    let result = tokio::select! { biased;
        _ = context.cancellation.cancelled() => return vec![],
        result = tokio::time::timeout(deadline(context), contributor.contribute(context)) => result,
    };
    match result {
        Ok(Ok(tools)) => tools,
        other => {
            failures.push(ContributorFailure {
                namespace: contributor.namespace().into(),
                reason: if other.is_err() {
                    "contributor timeout"
                } else {
                    "contributor error"
                }
                .into(),
                timed_out: other.is_err(),
            });
            vec![]
        }
    }
}

fn unbound_names(
    contributors: &[Arc<dyn ToolContributor>],
    bound: bool,
) -> Option<BTreeSet<String>> {
    if bound {
        return None;
    }
    let declared: Vec<_> = contributors
        .iter()
        .filter_map(|contributor| contributor.unbound_tool_names())
        .collect();
    (!declared.is_empty()).then(|| declared.into_iter().flatten().collect())
}

async fn register_tools(
    collected: Vec<(String, Arc<dyn Tool>)>,
    guards: &[Arc<dyn ToolGuard>],
    context: &ToolContributionContext,
    allowed: Option<BTreeSet<String>>,
    failed: Vec<ContributorFailure>,
) -> HostResult<StagedTools> {
    let mut registry = ToolRegistry::new();
    let mut namespaces = BTreeMap::new();
    let mut skipped = vec![];
    for (namespace, tool) in collected {
        let name = tool.definition().name.clone();
        if allowed.as_ref().is_some_and(|names| !names.contains(&name)) {
            continue;
        }
        let snapshot = ToolGuardContext {
            contribution: context.clone(),
            namespace: namespace.clone(),
            tool: tool.definition().clone(),
            arguments: None,
        };
        if !visible(guards, &snapshot).await {
            continue;
        }
        let guarded = Arc::new(GuardedTool {
            tool,
            guards: guards.to_vec(),
            snapshot,
            consumed: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
        });
        if registry.register(guarded).is_ok() {
            namespaces.insert(name, namespace);
        } else {
            skipped.push(name);
        }
    }
    Ok(StagedTools {
        registry: Arc::new(registry),
        namespaces,
        failed,
        skipped,
    })
}

async fn visible(guards: &[Arc<dyn ToolGuard>], context: &ToolGuardContext) -> bool {
    for guard in guards {
        let result = tokio::select! { biased;
            _ = context.contribution.cancellation.cancelled() => return false,
            result = tokio::time::timeout(deadline(&context.contribution), guard.is_visible(context)) => result,
        };
        if !matches!(result, Ok(Ok(true))) {
            return false;
        }
    }
    true
}

struct GuardedTool {
    tool: Arc<dyn Tool>,
    guards: Vec<Arc<dyn ToolGuard>>,
    snapshot: ToolGuardContext,
    /// Call ids whose one-use approval THIS attempt already spent (one
    /// wrapper per staged registry, so one per attempt): a second execute
    /// of the same call id goes back to asking.
    consumed: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
}

#[async_trait]
impl Tool for GuardedTool {
    fn definition(&self) -> &ToolDefinition {
        self.tool.definition()
    }
    async fn execute(&self, context: ToolContext, input: Value) -> Result<ToolResult, CoreError> {
        let staged = &self.snapshot.contribution;
        if context.chat_id != staged.chat_id
            || context.run_id != staged.run_id
            || context.scope_id != staged.scope_id
        {
            return Ok(guard_refusal(
                &context,
                "Tool invocation does not match staged run",
            ));
        }
        // Durable ownership first (ADR 0020): a recorded completion replays
        // and an uncertain operation is refused before any guard or body can
        // act. `durable: None` is the non-durable trusted-host mode,
        // mirroring the TypeScript optional durable bundle; chat execution
        // always wires it (stage_registry constructs it from the run's
        // ExecutionContext), so every host-served mutation is receipt-gated.
        let definition = self.tool.definition();
        let durable = staged.durable.clone();
        let effect_context = self.tool.effect_context();
        let mut uncertainty: Option<(ToolResult, String)> = None;
        if let Some(durable) = &durable {
            match durable
                .before_guards(
                    &context,
                    &self.snapshot,
                    definition,
                    effect_context.as_ref(),
                    &input,
                )
                .await?
            {
                crate::governance::PreGuard::Replay(result) => return Ok(result),
                // The operation is uncertain and no answer names this
                // generation. Do NOT refuse yet: fold it into the approval
                // flow below so the human can authorize the exact
                // unresolved execution — a denial from any guard still
                // wins, and a host that cannot park gets the honest
                // uncertain refusal as before.
                crate::governance::PreGuard::Uncertain {
                    refusal,
                    generation,
                } => {
                    uncertainty = Some((refusal, generation));
                }
                crate::governance::PreGuard::Proceed => {}
            }
        }
        // TS registry-staging rebuilds the guard context per call with the
        // live arguments (`arguments: snapshot`); guards that classify the
        // command text must see them, never a stale value.
        let call = ToolGuardContext {
            arguments: Some(input.clone()),
            ..self.snapshot.clone()
        };
        let mut approvable: Vec<ApprovalAsk> = vec![];
        for guard in &self.guards {
            let verdict = tokio::select! { biased;
                _ = context.cancellation.cancelled() => return Err(CoreError::new("cancelled", "aborted")),
                result = tokio::time::timeout(deadline(staged), guard.can_execute(&call)) => result,
            };
            match verdict {
                Ok(Ok(ToolGuardVerdict::Allowed)) => {}
                // Denial wins: a refusal no human may overrule ends the
                // call here, however many other guards offered an approval.
                Ok(Ok(ToolGuardVerdict::Refused(reason))) => {
                    return Ok(guard_refusal(&context, &reason));
                }
                Ok(Ok(ToolGuardVerdict::AwaitingHuman(ask))) => approvable.push(ask),
                _ => return Ok(guard_refusal(&context, "guard error")),
            }
        }
        if let Some((_, generation)) = &uncertainty {
            // The wrapper — not any guard — raises this ask: it is the
            // only party that knows the exact unresolved generation.
            approvable.push(ApprovalAsk {
                reason: format!("uncertain:{generation}"),
                preview: Some(format!(
                    "The previous run of {} was admitted without a recorded result; its effect is unknown. Approve running it again?",
                    definition.name
                )),
                requested_mode: None,
                uncertainty_generation: Some(generation.clone()),
            });
        }
        if !approvable.is_empty()
            && let Some(result) = self
                .resolve_approval(
                    &context,
                    definition,
                    &input,
                    &approvable,
                    uncertainty.as_ref(),
                )
                .await?
        {
            return Ok(result);
        }
        if let Some(guard) = &context.dispatch_guard {
            guard.check().await?;
        }
        // Admission after the guard chain, before the body: only a newly
        // admitted receipt may invoke it, and the store transaction has
        // already ended.
        let mut execution_id = None;
        if let Some(durable) = &durable {
            match durable
                .admit(
                    &context,
                    &self.snapshot,
                    definition,
                    effect_context.as_ref(),
                    &input,
                )
                .await?
            {
                crate::governance::Admission::Skipped => {}
                crate::governance::Admission::Granted { execution_id: id } => {
                    execution_id = Some(id);
                }
                crate::governance::Admission::Refused(result) => return Ok(result),
            }
        }
        if context.cancellation.is_cancelled() {
            return Err(CoreError::new("cancelled", "aborted"));
        }
        let result = self.tool.execute(context, input).await;
        // Persist the result before delivery. A body error leaves the receipt
        // admitted: whether the external effect landed is unknown, and the
        // retry path must refuse it as uncertain rather than re-execute.
        if let (Some(durable), Some(id)) = (&durable, execution_id.as_deref())
            && let Ok(tool_result) = &result
        {
            durable.complete(id, tool_result).await?;
        }
        result
    }
}

impl GuardedTool {
    /// Decide an approvable call (port of the TS wrapper's approval branch):
    /// spend the call's one-use answered approval, or run under an active
    /// standing grant, or ask — assembling the durable request here, where
    /// the arguments, the approvals, and the grants all live. `Some` ends
    /// the call (refusal or inert park); `None` lets it proceed to
    /// admission and body.
    async fn resolve_approval(
        &self,
        context: &ToolContext,
        definition: &agentkit_core::ToolDefinition,
        input: &Value,
        approvable: &[ApprovalAsk],
        uncertainty: Option<&(ToolResult, String)>,
    ) -> Result<Option<ToolResult>, CoreError> {
        let durable = self.snapshot.contribution.durable.clone();
        let args = crate::governance::args_sha256(input);
        // 1. One-use answered approval (exact call, exact arguments). For
        //    an uncertain reissue the grant must also name the exact
        //    unresolved generation; any other grant stays unspendable and
        //    the call asks afresh.
        let spendable = context
            .tool_call_id
            .as_deref()
            .and_then(|call_id| {
                durable
                    .as_ref()?
                    .approvals
                    .get(call_id)
                    .filter(|grant| grant.args_sha256 == args)
                    .filter(|grant| match &uncertainty {
                        Some((_, generation)) => {
                            grant.uncertainty_generation.as_deref() == Some(generation.as_str())
                        }
                        None => true,
                    })
                    .map(|grant| (call_id.to_owned(), grant.clone()))
            })
            .filter(|(call_id, _)| {
                !self
                    .consumed
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .contains(call_id)
            });
        if let Some((call_id, _grant)) = spendable {
            // Asked only after the one-use approval failed, and consumed
            // exactly once: a second execute of this call id asks again.
            self.consumed
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(call_id);
            return Ok(None);
        }
        // 2. A remembered approval of this exact call runs it like a spent
        //    grant. The policy refuses to key uncoverable calls (high-risk,
        //    plan, uncertain), so a remembered yes never erases a special
        //    refusal — and a reissue of an UNCERTAIN effect never runs on
        //    standing authority at all: only an explicit fresh answer that
        //    names the unresolved generation can open it. A call that
        //    already carries its grant (an exact duplicate, even after the
        //    grant was consumed) never consults the policy: denials and
        //    duplicates cannot depend on the grant-policy provider being
        //    available.
        let mut standing_key = None;
        let is_duplicate = context.tool_call_id.as_deref().is_some_and(|call_id| {
            durable
                .as_ref()
                .is_some_and(|durable| durable.approvals.contains_key(call_id))
        });
        if uncertainty.is_none()
            && !is_duplicate
            && let Some(durable) = &durable
            && let Some(policy) = &durable.standing
        {
            let reasons: Vec<String> = approvable.iter().map(|ask| ask.reason.clone()).collect();
            // A policy that could not answer has not remembered anything:
            // the call asks like any other.
            standing_key = policy
                .key_for(&self.snapshot_with_args(input), &reasons)
                .await
                .unwrap_or(None);
            if let Some((digest, _)) = &standing_key
                && has_active_standing_grant(durable, definition, digest).await?
            {
                return Ok(None);
            }
        }
        // 3. Ask — unless this host cannot ask. Without a park slot the
        //    approvable refusal simply stands as the final refusal, exactly
        //    as the TS wrapper returns its reason when `ctx.suspend` is
        //    absent; for an uncertain call that final answer is the honest
        //    uncertainty refusal (which names the generation), never a
        //    generic guard message.
        let spec = approval_spec(context, definition, approvable, args, standing_key);
        if context.request_human(spec) {
            // Inert: the loop takes the slot right after this returns,
            // records `run.waiting`, and parks the turn before admission —
            // no effect started, so no receipt can be uncertain.
            return Ok(Some(guard_refusal(context, "awaiting human approval")));
        }
        if let Some((refusal, _)) = uncertainty {
            return Ok(Some(refusal.clone()));
        }
        Ok(Some(guard_refusal(context, &approvable[0].reason)))
    }

    fn snapshot_with_args(&self, input: &Value) -> ToolGuardContext {
        ToolGuardContext {
            arguments: Some(input.clone()),
            ..self.snapshot.clone()
        }
    }
}

/// The durable approval request the answer path will settle. Content and
/// hash mirror the TypeScript projection field-for-field and in order:
/// the same call, in the same ask state, parks the same request bytes.
/// Identity: `(run, tool call)` for an ordinary ask, plus the uncertainty
/// generation for an uncertain reissue. The extra component is load-
/// bearing, not decoration: the SAME run can present a second uncertain
/// generation for one tool-call identity (answer generation N → the
/// approved execution is admitted → a crash leaves that admission
/// unresolved as generation N+1). The store keys the human-request row by
/// input id and refuses same-id-different-content rewrites, so an id
/// derived only from run + call would make the second ask a content-hash
/// COLLISION against the already-answered first row: the park would die
/// `park_failed` and the human could never be asked about N+1. With the
/// generation in the id each ask gets its own row, its own answer, and
/// its own hash. Safety is unchanged by the shape: a generation's answer
/// still authorizes exactly its own reissue — the one-use grant is spent
/// at admission (UNIQUE approval index), and both the host's `covers`
/// gate and the store's pending-generation gate compare the grant's
/// `uncertainty_generation` against the unresolved receipt's execution id
/// — so generation N's answer can never spend itself on generation N+1.
fn approval_spec(
    context: &ToolContext,
    definition: &agentkit_core::ToolDefinition,
    approvable: &[ApprovalAsk],
    args_sha256: String,
    standing: Option<(String, String)>,
) -> agentkit_core::HumanRequestSpec {
    let tool_call_id = context
        .tool_call_id
        .clone()
        .unwrap_or_else(|| "unknown".to_owned());
    // The exact unresolved execution an uncertain reissue asks about; an
    // answer is bound to this generation (it is part of the hashed
    // envelope AND of the request identity), so answering an older
    // uncertainty can never authorize a newer one — nor collide with it.
    let uncertainty = approvable
        .iter()
        .find_map(|ask| ask.uncertainty_generation.clone());
    let request_id = match &uncertainty {
        Some(generation) => format!("hreq:{}:{tool_call_id}:{generation}", context.run_id),
        None => format!("hreq:{}:{tool_call_id}", context.run_id),
    };
    let prompt = approvable
        .iter()
        .find_map(|ask| ask.preview.clone())
        .unwrap_or_else(|| format!("Approve {}?", definition.name));
    let requested_mode = approvable.iter().find_map(|ask| ask.requested_mode.clone());
    let mut content = serde_json::Map::new();
    content.insert("prompt".into(), prompt.clone().into());
    content.insert("toolName".into(), definition.name.clone().into());
    content.insert("kind".into(), "approval".into());
    content.insert("argsSha256".into(), args_sha256.clone().into());
    if let Some(generation) = &uncertainty {
        content.insert("uncertaintyGeneration".into(), generation.clone().into());
    }
    if let Some(mode) = &requested_mode {
        content.insert("requestedMode".into(), mode.clone().into());
    }
    if let Some((digest, label)) = &standing {
        content.insert(
            "standing".into(),
            serde_json::json!({"digest": digest, "label": label}),
        );
    }
    // `contentHash`: sha-256 over exactly the TS envelope, insertion order
    // included (preserve_order keeps it).
    let mut envelope = serde_json::Map::new();
    envelope.insert("requestId".into(), request_id.clone().into());
    envelope.insert("toolCallId".into(), tool_call_id.clone().into());
    envelope.insert("prompt".into(), prompt.into());
    // Guard-issued approvals never expire (the TS wrapper sets no
    // `expiresAt` either; only tools that suspend with their own request
    // may carry one).
    envelope.insert("kind".into(), "approval".into());
    envelope.insert("argsSha256".into(), args_sha256.into());
    if let Some(generation) = &uncertainty {
        envelope.insert("uncertaintyGeneration".into(), generation.clone().into());
    }
    if let Some(mode) = &requested_mode {
        envelope.insert("requestedMode".into(), mode.clone().into());
    }
    if let Some((digest, label)) = &standing {
        envelope.insert(
            "standing".into(),
            serde_json::json!({"digest": digest, "label": label}),
        );
    }
    let content_hash = super::durable_execution::sha256_hex(
        &serde_json::to_string(&Value::Object(envelope)).unwrap_or_else(|_| "{}".into()),
    );
    agentkit_core::HumanRequestSpec {
        request_id,
        content: Value::Object(content),
        content_hash,
        expires_at: None,
    }
}

/// True when the session carries an active (unexpired, unrevoked) standing
/// grant for this exact digest.
async fn has_active_standing_grant(
    durable: &crate::governance::DurableExecution,
    definition: &agentkit_core::ToolDefinition,
    digest: &str,
) -> Result<bool, CoreError> {
    let chat = durable
        .context
        .store
        .get_chat(&durable.chat_id)
        .await
        .map_err(|error| {
            CoreError::new(
                "recovery_history_unreadable",
                &format!("Cannot read standing approvals; the call stays gated. ({error})"),
            )
        })?
        .ok_or_else(|| CoreError::new("invalid_execution", "Approval chat vanished mid-run"))?;
    let _ = definition;
    Ok(agentkit_store::find_active_standing_grant(&chat.metadata, digest).is_some())
}

fn guard_refusal(context: &ToolContext, reason: &str) -> ToolResult {
    let data = serde_json::json!({"errorCode":"tool_guard_refused","errorMessage":reason,"phase":"guard","retryable":false});
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

fn validate_namespace(contributor: &dyn ToolContributor) -> HostResult<()> {
    let name = contributor.namespace();
    let valid = name.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        });
    if !valid {
        return Err(HostError::Execution("tool_namespace_invalid".into()));
    }
    if !contributor.privileged() && ["agentkit", "chat", "mcp"].contains(&name) {
        return Err(HostError::Execution("tool_namespace_reserved".into()));
    }
    Ok(())
}

fn deadline(context: &ToolContributionContext) -> Duration {
    Duration::from_millis(context.timeout_ms.max(1))
}
