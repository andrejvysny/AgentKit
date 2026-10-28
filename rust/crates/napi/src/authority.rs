//! Rust-owned authority for JS-contributed tools.
//!
//! The binding never implements a second policy engine: the JS tool guard
//! reads the session's durable execution mode through the coding pack's own
//! `SessionExecutionAuthority` and reproduces the coding pack's verdict
//! words byte-for-byte, escalating only through the host's
//! `PlanApprovalPolicy`. A JS write tool therefore cannot act in plan mode
//! without a human approval, and the escalation grant is the host's
//! durable record, not binding memory.

use std::sync::OnceLock;

use agentkit_host::{
    AgentSessionService, ApprovalAsk, HostResult, PlanApprovalPolicy, PlanAuthorization,
    PlanGrantIntent, ToolGuard, ToolGuardContext, ToolGuardVerdict,
};
use agentkit_toolpacks_coding::types::{CodingMode, CodingPermissionAuthority, PermissionContext};
use agentkit_toolpacks_coding::{PLAN_MODE_BLOCKED_HINT, SessionExecutionAuthority};
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
        // Read tools run free in every mode; effect is declared, validated,
        // and part of the replay identity (a redefinition invalidates
        // receipts), so this is the same axis the host classifies on.
        if context.tool.effect == "read" {
            return Ok(ToolGuardVerdict::Allowed);
        }
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
        if mode == CodingMode::Plan {
            if self.is_plan_tool(&name) {
                return Ok(ToolGuardVerdict::AwaitingHuman(ApprovalAsk {
                    reason: format!("plan_approval_required: {name}"),
                    preview: plan_preview(context.arguments.as_ref()),
                    requested_mode: plan_requested_mode(context.arguments.as_ref()),
                    uncertainty_generation: None,
                }));
            }
            return Ok(ToolGuardVerdict::Refused(format!(
                "{PLAN_MODE_BLOCKED_HINT} ({name} is a write tool)"
            )));
        }
        if mode == CodingMode::ReadOnly {
            return Ok(ToolGuardVerdict::Refused(format!(
                "read_only: {name} is not allowed"
            )));
        }
        if mode == CodingMode::Ask {
            return Ok(ToolGuardVerdict::AwaitingHuman(ApprovalAsk {
                reason: format!("approval_required: {name}"),
                preview: None,
                requested_mode: None,
                uncertainty_generation: None,
            }));
        }
        Ok(ToolGuardVerdict::Allowed)
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
