use crate::{
    ApprovalAsk, ContributedTool, GovernanceError, GovernanceResult, ToolContributionContext,
    ToolContributor, ToolGuard, ToolGuardContext, ToolGuardVerdict,
};
use agentkit_core::{
    CoreError, Tool, ToolContext, ToolDefinition, ToolRegistry, ToolResult, failure_catalog,
};
use async_trait::async_trait;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};

mod card;
mod execution_recheck;

/// Tool namespaces no standing grant may cover: each call asks the owner
/// again (`memory_forget` deletes the owner's own notes; a remembered yes
/// must never become blanket authority to forget). A sandbox export is
/// approved on its own content hash and a network host by its own grant
/// (ADR 0038), so a remembered yes never covers a `sandbox` call either.
const NO_STANDING_NAMESPACES: [&str; 2] = ["memory", "sandbox"];

fn standing_eligible(context: &ToolGuardContext) -> bool {
    !NO_STANDING_NAMESPACES.contains(&context.namespace.as_str())
}

fn check_contribution(
    tool: &ContributedTool,
    context: &ToolContributionContext,
) -> GovernanceResult<Arc<dyn Tool>> {
    match tool {
        ContributedTool::Read(tool) if tool.definition().effect == "read" => Ok(tool.clone()),
        ContributedTool::DirectWrite(tool) if tool.definition().effect == "write" => {
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
        ContributedTool::Write(tool)
            if tool.matches_actor(&context.actor, context.actor_id.as_deref()) =>
        {
            Ok(tool.clone())
        }
        _ => Err(GovernanceError::ToolEffectOrActorMismatch),
    }
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
    fn toolsets(&self) -> Vec<agentkit_core::Toolset> {
        self.registry.toolsets()
    }
    fn legacy_names(&self) -> Vec<agentkit_core::LegacyName> {
        self.registry.legacy_names()
    }
    async fn contribute(
        &self,
        _context: &ToolContributionContext,
    ) -> GovernanceResult<Vec<ContributedTool>> {
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
    fn effect_context(&self) -> Option<Value> {
        self.inner.effect_context()
    }
    fn records_command_start(&self) -> bool {
        self.inner.records_command_start()
    }
    fn loading(&self) -> agentkit_core::ToolLoading {
        self.inner.loading()
    }
    fn toolset(&self) -> Option<&str> {
        self.inner.toolset()
    }
    fn arg_aliases(&self) -> Vec<(String, String)> {
        self.inner.arg_aliases()
    }
    fn constraints(&self) -> Vec<agentkit_core::ToolConstraint> {
        self.inner.constraints()
    }
    fn check_constraints(&self, input: &Value) -> Option<String> {
        self.inner.check_constraints(input)
    }
    async fn execute(&self, context: ToolContext, input: Value) -> Result<ToolResult, CoreError> {
        self.inner.execute(context, input).await
    }
}

pub struct ContributorFailure {
    pub namespace: String,
    pub reason: String,
    pub timed_out: bool,
}

/// A tool a guard or an unbound target hid from the registry. The host keeps
/// its definition in the offered tools and refuses a call to it with `reason`
/// (roadmap 1.3: guards decide at call time, they do not shrink the array).
pub struct WithheldTool {
    pub definition: ToolDefinition,
    pub reason: String,
}

pub struct StagedTools {
    pub registry: Arc<ToolRegistry>,
    pub withheld: Vec<WithheldTool>,
    pub namespaces: BTreeMap<String, String>,
    pub failed: Vec<ContributorFailure>,
    pub skipped: Vec<String>,
}

pub async fn stage_tools(
    contributors: &[Arc<dyn ToolContributor>],
    guards: &[Arc<dyn ToolGuard>],
    context: ToolContributionContext,
) -> GovernanceResult<StagedTools> {
    for contributor in contributors {
        validate_namespace(contributor.as_ref())?;
    }
    let mut failed = vec![];
    let mut owners = BTreeMap::new();
    let mut collected = vec![];
    for (index, contributor) in contributors.iter().enumerate() {
        let tools = contribute(contributor.as_ref(), &context, &mut failed).await;
        for tool in tools {
            let tool = check_contribution(&tool, &context)?;
            let name = &tool.definition().name;
            if owners.get(name).is_some_and(|owner| *owner != index) {
                return Err(GovernanceError::ToolNameCollision);
            }
            owners.insert(name.clone(), index);
            collected.push((
                contributor.namespace().to_owned(),
                tool,
                contributor.privileged(),
            ));
        }
    }
    let allowed = unbound_names(contributors, context.has_primary_binding);
    let toolsets = contributors
        .iter()
        .flat_map(|contributor| contributor.toolsets())
        .collect();
    let legacy = contributors
        .iter()
        .flat_map(|contributor| contributor.legacy_names())
        .collect();
    register_tools(
        collected, guards, &context, allowed, failed, toolsets, legacy,
    )
    .await
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
    collected: Vec<(String, Arc<dyn Tool>, bool)>,
    guards: &[Arc<dyn ToolGuard>],
    context: &ToolContributionContext,
    allowed: Option<BTreeSet<String>>,
    failed: Vec<ContributorFailure>,
    toolsets: Vec<agentkit_core::Toolset>,
    legacy: Vec<agentkit_core::LegacyName>,
) -> GovernanceResult<StagedTools> {
    let mut registry = ToolRegistry::new();
    for toolset in toolsets {
        registry.register_toolset(toolset);
    }
    let mut namespaces = BTreeMap::new();
    let mut skipped = vec![];
    let mut withheld = vec![];
    for (namespace, tool, privileged) in collected {
        let name = tool.definition().name.clone();
        // A privileged host contributor's tools are dynamic per turn and need
        // no bound target, so they cannot be declared up front; unbound
        // narrowing never prunes them (their guards still decide each call).
        if !privileged && allowed.as_ref().is_some_and(|names| !names.contains(&name)) {
            withheld.push(WithheldTool {
                definition: tool.definition().clone(),
                reason: "this session has no bound target for it".into(),
            });
            continue;
        }
        let snapshot = ToolGuardContext {
            contribution: context.clone(),
            namespace: namespace.clone(),
            tool: tool.definition().clone(),
            arguments: None,
        };
        if !visible(guards, &snapshot).await {
            withheld.push(WithheldTool {
                definition: tool.definition().clone(),
                reason: "a guard does not allow it in this session".into(),
            });
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
    // After the tools, so a name a contributor still registers wins.
    for name in legacy {
        registry.register_legacy_name(name);
    }
    Ok(StagedTools {
        registry: Arc::new(registry),
        withheld,
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

/// An earlier execution of this operation without a durable result, as
/// the ledger reported it before the guards ran.
enum Unresolved {
    /// No answer on an uncertainty card names it: the call asks about
    /// `generation`, or returns `refusal` when this host cannot ask.
    Ask {
        refusal: Box<ToolResult>,
        generation: String,
        arguments_unknown: bool,
    },
    /// The answer this call carries names exactly `generation`, on an
    /// uncertainty card.
    Answered { generation: String },
}

impl Unresolved {
    fn generation(&self) -> &str {
        match self {
            Self::Ask { generation, .. } | Self::Answered { generation } => generation,
        }
    }
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
    fn loading(&self) -> agentkit_core::ToolLoading {
        self.tool.loading()
    }
    fn toolset(&self) -> Option<&str> {
        self.tool.toolset()
    }
    fn arg_aliases(&self) -> Vec<(String, String)> {
        self.tool.arg_aliases()
    }
    fn constraints(&self) -> Vec<agentkit_core::ToolConstraint> {
        self.tool.constraints()
    }
    fn check_constraints(&self, input: &Value) -> Option<String> {
        self.tool.check_constraints(input)
    }
    async fn record_schema_refusal(&self, context: &ToolContext, args: &Value, message: &str) {
        // The durable wrapper owns the receipt ledger; without it this host
        // mode records nothing (the trusted-host mirror of `durable: None`).
        let Some(durable) = &self.snapshot.contribution.durable else {
            return;
        };
        let snapshot = ToolGuardContext {
            arguments: Some(args.clone()),
            ..self.snapshot.clone()
        };
        durable
            .record_schema_refusal(
                context,
                &snapshot,
                self.tool.definition(),
                self.tool.effect_context().as_ref(),
                args,
                message,
            )
            .await;
    }
    async fn execute(
        &self,
        mut context: ToolContext,
        input: Value,
    ) -> Result<ToolResult, CoreError> {
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
        let mut unresolved: Option<Unresolved> = None;
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
                crate::PreGuard::Replay(result) | crate::PreGuard::Refused(result) => {
                    return Ok(result);
                }
                // The operation is uncertain and no answer names this
                // generation. Do NOT refuse yet: fold it into the approval
                // flow below so the human can authorize the exact
                // unresolved execution — a denial from any guard still
                // wins, and a host that cannot park gets the honest
                // uncertain refusal as before.
                crate::PreGuard::Uncertain {
                    refusal,
                    generation,
                    arguments_unknown,
                } => {
                    unresolved = Some(Unresolved::Ask {
                        refusal: Box::new(refusal),
                        generation,
                        arguments_unknown,
                    });
                }
                // The call's answer names the pending generation on an
                // uncertainty card, which showed the guards' asks too: a
                // guard that asks again on resume is answered by that yes.
                crate::PreGuard::Reauthorized { generation } => {
                    unresolved = Some(Unresolved::Answered { generation });
                }
                crate::PreGuard::Proceed => {}
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
        if let Some(Unresolved::Ask {
            generation,
            arguments_unknown,
            ..
        }) = &unresolved
        {
            // The wrapper — not any guard — raises this ask: it is the
            // only party that knows the exact unresolved generation. It
            // leads the card, and the other guards' previews follow it
            // (`card::approval_spec`): one yes answers every ask on the
            // card, so the owner sees all of them first.
            approvable.insert(
                0,
                card::uncertainty_ask(&definition.name, generation, *arguments_unknown),
            );
        }
        let mut standing_digest = None;
        if !approvable.is_empty()
            && let Some(result) = self
                .resolve_approval(
                    &context,
                    &input,
                    &approvable,
                    unresolved.as_ref(),
                    effect_context.as_ref(),
                    &mut standing_digest,
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
        // Cancellation is judged BEFORE admission: an Admitted receipt with
        // no body behind it reads as an uncertain effect on every retry.
        // Once admitted, the body runs.
        if context.cancellation.is_cancelled() {
            return Err(CoreError::new("cancelled", "aborted"));
        }
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
                crate::Admission::Skipped => {}
                crate::Admission::Granted { execution_id: id } => {
                    execution_id = Some(id);
                }
                crate::Admission::Refused(result) => return Ok(*result),
            }
        }
        if definition.capability == "process.exec" {
            context.dispatch_guard = Some(Arc::new(execution_recheck::ExecutionRecheck {
                original: context.dispatch_guard.clone(),
                guards: self.guards.clone(),
                snapshot: call,
                approvals: approvable,
                standing_digest,
                cancellation: context.cancellation.clone(),
            }));
        }
        super::execution_completion::execute_receipted(
            self.tool.clone(),
            durable,
            execution_id,
            context,
            input,
        )
        .await
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
        input: &Value,
        approvable: &[ApprovalAsk],
        unresolved: Option<&Unresolved>,
        effect_context: Option<&Value>,
        selected_standing_digest: &mut Option<String>,
    ) -> Result<Option<ToolResult>, CoreError> {
        let definition = self.tool.definition();
        let durable = self.snapshot.contribution.durable.clone();
        let args = crate::args_sha256(input);
        let context_hash = durable.as_ref().map(|durable| {
            durable.execution_context_sha256(
                &self.snapshot.contribution,
                definition,
                effect_context,
            )
        });
        if context
            .tool_call_id
            .as_deref()
            .and_then(|id| durable.as_ref()?.approval_grant(id))
            .and_then(|grant| grant.context_sha256)
            .is_some_and(|hash| Some(hash.as_str()) != context_hash.as_deref())
        {
            return Ok(Some(guard_refusal(
                context,
                "The approval no longer matches this execution context.",
            )));
        }
        // 1. One-use answered approval (exact call, exact arguments). For
        //    an uncertain reissue (asked about now, or already answered
        //    when a guard asks again) the grant must also name the exact
        //    unresolved generation, on an uncertainty card; any other grant
        //    stays unspendable and the call asks afresh.
        let generation = unresolved.map(Unresolved::generation);
        let spendable = context
            .tool_call_id
            .as_deref()
            .and_then(|call_id| {
                durable
                    .as_ref()?
                    .approval_grant(call_id)
                    .filter(|grant| grant.args_sha256 == args)
                    .filter(|grant| grant.uncertainty_generation.as_deref() == generation)
                    .filter(|grant| generation.is_none() || grant.uncertainty_card)
                    .map(|grant| (call_id.to_owned(), grant))
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
                .is_some_and(|durable| durable.approval_grant(call_id).is_some())
        });
        if unresolved.is_none()
            && !is_duplicate
            && standing_eligible(&self.snapshot)
            && let Some(durable) = &durable
            && let Some(policy) = durable.standing_policy()
        {
            let reasons: Vec<String> = approvable.iter().map(|ask| ask.reason.clone()).collect();
            // A policy that could not answer has not remembered anything:
            // the call asks like any other.
            standing_key = policy
                .key_for(&self.snapshot_with_args(input), &reasons)
                .await
                .unwrap_or(None);
            if let Some((digest, _)) = &standing_key
                && durable.has_active_standing_grant(digest).await?
            {
                *selected_standing_digest = Some(digest.clone());
                return Ok(None);
            }
        }
        // 3. Ask — unless this host cannot ask. Without a park slot the
        //    approvable refusal simply stands as the final refusal, exactly
        //    as the TS wrapper returns its reason when `ctx.suspend` is
        //    absent; for an uncertain call that final answer is the honest
        //    uncertainty refusal (which names the generation), never a
        //    generic guard message.
        let spec = card::approval_spec(
            context,
            definition,
            approvable,
            args,
            standing_key,
            context_hash,
        );
        if context.request_human(spec) {
            // Inert: the loop takes the slot right after this returns,
            // records `run.waiting`, and parks the turn before admission —
            // no effect started, so no receipt can be uncertain.
            return Ok(Some(guard_refusal(context, "awaiting human approval")));
        }
        if let Some(Unresolved::Ask { refusal, .. }) = unresolved {
            return Ok(Some(refusal.as_ref().clone()));
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

fn guard_refusal(context: &ToolContext, reason: &str) -> ToolResult {
    let failure = failure_catalog::from_refusal(reason);
    failure_catalog::failure_result(
        "tool_guard_refused",
        &failure,
        "guard",
        None,
        &context.limits,
    )
}

fn validate_namespace(contributor: &dyn ToolContributor) -> GovernanceResult<()> {
    let name = contributor.namespace();
    let valid = name.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        });
    if !valid {
        return Err(GovernanceError::ToolNamespaceInvalid);
    }
    if !contributor.privileged() && ["agentkit", "chat", "mcp"].contains(&name) {
        return Err(GovernanceError::ToolNamespaceReserved);
    }
    Ok(())
}

fn deadline(context: &ToolContributionContext) -> Duration {
    Duration::from_millis(context.timeout_ms.max(1))
}

#[cfg(test)]
mod standing_tests {
    use super::*;
    use agentkit_core::{CancellationToken, ToolLimits};
    use serde_json::json;

    fn context(namespace: &str, tool: &str) -> ToolGuardContext {
        ToolGuardContext {
            contribution: ToolContributionContext {
                chat_id: Some("chat".into()),
                run_id: "run".into(),
                scope_id: None,
                actor: Value::Null,
                actor_id: None,
                bindings: vec![],
                limits: ToolLimits::default(),
                cancellation: CancellationToken::new(),
                has_primary_binding: true,
                timeout_ms: 1_000,
                durable: None,
            },
            namespace: namespace.into(),
            tool: ToolDefinition {
                name: tool.into(),
                version: "1".into(),
                effect: "write".into(),
                capability: "memory.write".into(),
                description: String::new(),
                input_schema: json!({"type": "object"}),
                output_schema: None,
                timeout_ms: None,
                examples: vec![],
            },
            arguments: None,
        }
    }

    #[test]
    fn no_standing_grant_covers_the_memory_namespace() {
        assert!(!standing_eligible(&context("memory", "memory_forget")));
        assert!(standing_eligible(&context("coding", "run_command")));
    }

    #[test]
    fn no_standing_grant_covers_the_sandbox_namespace() {
        assert!(!standing_eligible(&context(
            "sandbox",
            "sandbox_export_file"
        )));
        assert!(!standing_eligible(&context("sandbox", "sandbox_run")));
    }
}
