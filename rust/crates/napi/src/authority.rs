//! Rust-owned authority for JS-contributed tools.
//!
//! The binding never implements a second policy engine: the JS tool guard
//! runs the ONE shared decision matrix (`agentkit_core::permission::decide`
//! — the same code path the coding pack's guard takes) against the
//! session's durable execution mode read through the host's
//! `SessionExecutionAuthority`, and reproduces the pack's verdict words
//! byte-for-byte, escalating only through the host's `PlanApprovalPolicy`.
//! A JS write tool therefore cannot act in plan mode without a human
//! approval, a command cannot act in `auto_edit` without one, and the
//! escalation grant is the host's durable record, not binding memory.

use std::sync::OnceLock;

use agentkit_core::permission::{
    BlockedFinal, CodingMode, PLAN_MODE_BLOCKED_HINT, PermissionDecision, ToolClass,
    classify_capability, decide,
};
use agentkit_host::{
    AgentSessionService, ApprovalAsk, CodingPermissionAuthority, HostResult, PermissionContext,
    PlanApprovalPolicy, PlanAuthorization, PlanGrantIntent, SessionExecutionAuthority, ToolGuard,
    ToolGuardContext, ToolGuardVerdict,
};
use async_trait::async_trait;
use serde_json::Value;

/// Namespace of JS-contributed tools; the guard must never judge the
/// coding pack's own namespace — those tools carry their own guard chain.
pub(crate) const JS_NAMESPACE: &str = "js";

/// Late-bound session service: the ChatExecutor (and therefore the guard)
/// is constructed before the session service exists, and the worker only
/// starts after `bind`, so a run can never observe an unbound authority.
/// Unbound means "no session-level decision yet": the guard falls back to
/// its configured mode instead of guessing.
pub(crate) struct BindingAuthority {
    service: OnceLock<AgentSessionService>,
}

impl BindingAuthority {
    pub(crate) fn new() -> Self {
        Self {
            service: OnceLock::new(),
        }
    }

    /// Called once during construction, before the worker starts.
    pub(crate) fn bind(&self, service: AgentSessionService) {
        let _ = self.service.set(service);
    }

    fn inner(&self) -> Option<SessionExecutionAuthority> {
        self.service
            .get()
            .cloned()
            .map(SessionExecutionAuthority::new)
    }
}

#[async_trait]
impl CodingPermissionAuthority for BindingAuthority {
    async fn mode_of(&self, ctx: &PermissionContext) -> HostResult<Option<CodingMode>> {
        match self.inner() {
            Some(authority) => authority.mode_of(ctx).await,
            None => Ok(None),
        }
    }

    async fn plan_approved(
        &self,
        chat_id: Option<&str>,
        plan_digest: &str,
    ) -> HostResult<Option<bool>> {
        match self.inner() {
            Some(authority) => authority.plan_approved(chat_id, plan_digest).await,
            None => Ok(None),
        }
    }
}

/// The plan-approvable JS tools and the mode the human's approval grants.
#[derive(Clone)]
pub(crate) struct PlanConfiguration {
    pub tool_names: Vec<String>,
    pub granted_mode: CodingMode,
}

/// The JS-tool permission guard: the same verdict words the coding pack
/// uses, applied to the `js` namespace by declared effect. Plan mode is
/// fail-closed for writes; the only escalation is a plan-tool approval the
/// host records durably before anything may change.
pub(crate) struct JsToolGuard {
    authority: std::sync::Arc<dyn CodingPermissionAuthority>,
    configured_mode: CodingMode,
    plan: PlanConfiguration,
}

impl JsToolGuard {
    pub(crate) fn new(
        authority: std::sync::Arc<dyn CodingPermissionAuthority>,
        configured_mode: CodingMode,
        plan: PlanConfiguration,
    ) -> Self {
        Self {
            authority,
            configured_mode,
            plan,
        }
    }

    fn is_plan_tool(&self, name: &str) -> bool {
        self.plan.tool_names.iter().any(|tool| tool == name)
    }
}

#[async_trait]
impl ToolGuard for JsToolGuard {
    async fn can_execute(&self, context: &ToolGuardContext) -> HostResult<ToolGuardVerdict> {
        if context.namespace != JS_NAMESPACE {
            return Ok(ToolGuardVerdict::Allowed);
        }
        // The SAME matrix the coding pack's guard runs (`decide`), with the
        // JS metadata adapted into its axes: capability classifies first —
        // a tool declaring `process.exec` is an Exec tool even if it
        // declares effect `write`; only unknown capabilities fall back to
        // the validated declared effect ("read"/"write").
        let tool_class = classify_capability(&context.tool.capability).unwrap_or_else(|| {
            if context.tool.effect == "read" {
                ToolClass::Read
            } else {
                ToolClass::Write
            }
        });
        let name = context.tool.name.clone();
        let chat = PermissionContext {
            chat_id: context.contribution.chat_id.clone(),
            run_id: Some(context.contribution.run_id.clone()),
        };
        let mode = self
            .authority
            .mode_of(&chat)
            .await?
            .unwrap_or(self.configured_mode);
        match decide(mode, tool_class, self.is_plan_tool(&name)) {
            PermissionDecision::Allowed => Ok(ToolGuardVerdict::Allowed),
            PermissionDecision::PlanApprovalRequired => {
                Ok(ToolGuardVerdict::AwaitingHuman(ApprovalAsk {
                    reason: format!("plan_approval_required: {name}"),
                    preview: plan_preview(context.arguments.as_ref()),
                    requested_mode: plan_requested_mode(context.arguments.as_ref()),
                    uncertainty_generation: None,
                }))
            }
            PermissionDecision::BlockedFinal(BlockedFinal::PlanMode { tool_class }) => {
                Ok(ToolGuardVerdict::Refused(format!(
                    "{PLAN_MODE_BLOCKED_HINT} ({name} is a {} tool)",
                    tool_class.as_str()
                )))
            }
            PermissionDecision::BlockedFinal(BlockedFinal::ReadOnly) => Ok(
                ToolGuardVerdict::Refused(format!("read_only: {name} is not allowed")),
            ),
            PermissionDecision::ApprovalRequired => {
                Ok(ToolGuardVerdict::AwaitingHuman(ApprovalAsk {
                    reason: format!("approval_required: {name}"),
                    preview: None,
                    requested_mode: None,
                    uncertainty_generation: None,
                }))
            }
        }
    }
}

/// The host's plan-approval policy for JS plan tools: approving moves the
/// session to the configured grant mode (never back into plan).
pub(crate) struct BindingPlanPolicy {
    plan: PlanConfiguration,
}

impl BindingPlanPolicy {
    pub(crate) fn new(plan: PlanConfiguration) -> Self {
        Self { plan }
    }
}

#[async_trait]
impl PlanApprovalPolicy for BindingPlanPolicy {
    fn tool_names(&self) -> Vec<String> {
        self.plan.tool_names.clone()
    }

    async fn granted_mode(&self, _session_id: &str) -> HostResult<String> {
        Ok(self.plan.granted_mode.as_str().to_owned())
    }

    async fn authorize(&self, _intent: &PlanGrantIntent<'_>) -> HostResult<PlanAuthorization> {
        Ok(PlanAuthorization::Allowed)
    }
}

/// Byte-parity with the coding pack's plan preview (guard.rs keeps these
/// private; the human must approve on the same text either way).
fn plan_requested_mode(args: Option<&Value>) -> Option<String> {
    args?.get("mode")?.as_str().map(str::to_owned)
}

fn plan_preview(args: Option<&Value>) -> Option<String> {
    let args = args?;
    let plan = args.get("plan")?.as_str()?;
    if plan.trim().is_empty() {
        return None;
    }
    Some(match args.get("mode").and_then(Value::as_str) {
        Some(mode) => format!("{plan}\n\n(requested mode: {mode})"),
        None => plan.to_owned(),
    })
}
