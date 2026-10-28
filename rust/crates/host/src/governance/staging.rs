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
        // TS registry-staging rebuilds the guard context per call with the
        // live arguments (`arguments: snapshot`); guards that classify the
        // command text must see them, never a stale value.
        let call = ToolGuardContext {
            arguments: Some(input.clone()),
            ..self.snapshot.clone()
        };
        for guard in &self.guards {
            let verdict = tokio::select! { biased;
                _ = context.cancellation.cancelled() => return Err(CoreError::new("cancelled", "aborted")),
                result = tokio::time::timeout(deadline(staged), guard.can_execute(&call)) => result,
            };
            match verdict {
                Ok(Ok(ToolGuardVerdict::Allowed)) => {}
                Ok(Ok(ToolGuardVerdict::Refused(reason))) => {
                    return Ok(guard_refusal(&context, &reason));
                }
                _ => return Ok(guard_refusal(&context, "guard error")),
            }
        }
        if let Some(guard) = &context.dispatch_guard {
            guard.check().await?;
        }
        if context.cancellation.is_cancelled() {
            return Err(CoreError::new("cancelled", "aborted"));
        }
        self.tool.execute(context, input).await
    }
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
