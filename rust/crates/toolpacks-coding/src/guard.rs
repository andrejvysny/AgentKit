//! Coding permission guard. Port of `packages/toolpacks-coding/src/guard.ts`.
//!
//! The role ceilings (`ROLE_CEILING`) are adapted from OneMind
//! (`src-ts/src/code/tool-authority.ts`, MIT). Modes and the `ToolGuard`
//! port are AgentKit's own.
//!
//! The approvable verdicts (`approval_required:`, `plan_approval_required:`)
//! carry their reason, preview, and requested mode in
//! [`ToolGuardVerdict::AwaitingHuman`]; the host wrapper turns one into the
//! durable human request (ADR 0019). Reason strings stay byte-identical to
//! TypeScript; final refusals (role ceiling, plan-mode blocks) carry no
//! approval and cannot be answered open.

use std::sync::Arc;

use serde_json::Value;

use agentkit_host::{ApprovalAsk, HostResult, ToolGuard, ToolGuardContext, ToolGuardVerdict};
use async_trait::async_trait;

use crate::plan_tool::{SUBMIT_PLAN_CAPABILITY, SUBMIT_PLAN_TOOL_NAME};
use crate::types::{CodingMode, CodingPermissionAuthority, CodingRole, PermissionContext};

/// The namespace the coding contributor registers under.
pub const CODING_NAMESPACE: &str = "coding";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ToolClass {
    Read,
    Write,
    Exec,
}

impl ToolClass {
    fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Exec => "exec",
        }
    }
}

fn class_by_capability(capability: &str) -> Option<ToolClass> {
    Some(match capability {
        "fs.read" => ToolClass::Read,
        "git.read" => ToolClass::Read,
        "fs.write" => ToolClass::Write,
        "process.exec" => ToolClass::Exec,
        // `submit_plan` has no side effects; its whole effect is the human
        // decision.
        SUBMIT_PLAN_CAPABILITY => ToolClass::Read,
        _ => return None,
    })
}

fn role_ceiling(role: CodingRole) -> &'static [ToolClass] {
    match role {
        CodingRole::Supervisor => &[ToolClass::Read],
        CodingRole::Reviewer => &[ToolClass::Read, ToolClass::Exec],
        CodingRole::Implementer => &[ToolClass::Read, ToolClass::Write, ToolClass::Exec],
    }
}

fn mode_allows(mode: CodingMode) -> &'static [ToolClass] {
    match mode {
        CodingMode::ReadOnly | CodingMode::Ask | CodingMode::Plan => &[ToolClass::Read],
        CodingMode::AutoEdit => &[ToolClass::Read, ToolClass::Write],
        CodingMode::FullAuto => &[ToolClass::Read, ToolClass::Write, ToolClass::Exec],
    }
}

/// What the model is told when plan mode blocks it: the way out, by name.
pub const PLAN_MODE_BLOCKED_HINT: &str =
    "plan mode: propose the change with submit_plan and wait for the user to approve it.";

pub struct CodingPermissionGuardOptions {
    /// The mode to use when the session's own authority states none.
    pub mode: CodingMode,
    pub role: Option<CodingRole>,
    /// Host-owned per-session mode/role/plan state.
    pub authority: Option<Arc<dyn CodingPermissionAuthority>>,
    /// Tool names whose approval request is a PLAN approval.
    pub plan_tool_names: Option<Vec<String>>,
}

/// Judges only tools in the `coding` namespace; anything else gets no
/// opinion (`allowed: true`), because guards compose with AND. A role
/// ceiling always wins over the mode, and NO mode — including `full_auto` —
/// lifts one: a supervisor stays read-only because the ceiling is checked
/// first and its refusal carries no `approval`, so no human answer can
/// reopen it.
///
/// `ask` and `auto_edit` answer `approval_required` for the classes they
/// would need a human for. `plan` is stricter: writes and commands are
/// refused FINAL, because a per-call approval in plan mode would let the
/// model walk through the door one small edit at a time; escalation is a
/// plan the human approves as a whole, which the host records as a durable
/// grant.
pub struct CodingPermissionGuard {
    default_role: CodingRole,
    configured_mode: CodingMode,
    authority: Option<Arc<dyn CodingPermissionAuthority>>,
    plan_tools: Vec<String>,
}

impl CodingPermissionGuard {
    pub fn new(options: CodingPermissionGuardOptions) -> Self {
        Self {
            default_role: options.role.unwrap_or(CodingRole::Implementer),
            configured_mode: options.mode,
            authority: options.authority,
            plan_tools: options
                .plan_tool_names
                .unwrap_or_else(|| vec![SUBMIT_PLAN_TOOL_NAME.to_owned()]),
        }
    }
}

#[async_trait]
impl ToolGuard for CodingPermissionGuard {
    async fn can_execute(&self, context: &ToolGuardContext) -> HostResult<ToolGuardVerdict> {
        if context.namespace != CODING_NAMESPACE {
            return Ok(ToolGuardVerdict::Allowed);
        }
        let name = context.tool.name.clone();
        let Some(tool_class) = class_by_capability(&context.tool.capability) else {
            return Ok(ToolGuardVerdict::Refused(format!(
                "unknown_capability: {} ({})",
                name, context.tool.capability
            )));
        };
        let chat = PermissionContext {
            chat_id: context.contribution.chat_id.clone(),
            run_id: Some(context.contribution.run_id.clone()),
        };
        let role = match &self.authority {
            Some(authority) => authority.role_of(&chat).await?.unwrap_or(self.default_role),
            None => self.default_role,
        };
        if !role_ceiling(role).contains(&tool_class) {
            return Ok(ToolGuardVerdict::Refused(format!(
                "role_ceiling: {} may not use {} tools ({})",
                role.as_str(),
                tool_class.as_str(),
                name
            )));
        }
        let mode = match &self.authority {
            Some(authority) => authority
                .mode_of(&chat)
                .await?
                .unwrap_or(self.configured_mode),
            None => self.configured_mode,
        };
        if mode == CodingMode::Plan {
            if tool_class == ToolClass::Read {
                // Reads run free; only the plan submission itself needs the
                // human.
                if self.plan_tools.iter().any(|plan| plan == &name) {
                    return Ok(ToolGuardVerdict::AwaitingHuman(ApprovalAsk {
                        reason: format!("plan_approval_required: {name}"),
                        preview: plan_preview(context.arguments.as_ref()),
                        requested_mode: plan_requested_mode(context.arguments.as_ref()),
                        uncertainty_generation: None,
                    }));
                }
                return Ok(ToolGuardVerdict::Allowed);
            }
            return Ok(ToolGuardVerdict::Refused(format!(
                "{PLAN_MODE_BLOCKED_HINT} ({name} is a {} tool)",
                tool_class.as_str()
            )));
        }
        if mode_allows(mode).contains(&tool_class) {
            return Ok(ToolGuardVerdict::Allowed);
        }
        if mode == CodingMode::ReadOnly {
            return Ok(ToolGuardVerdict::Refused(format!(
                "read_only: {name} is not allowed"
            )));
        }
        Ok(ToolGuardVerdict::AwaitingHuman(ApprovalAsk {
            reason: format!("approval_required: {name}"),
            preview: None,
            requested_mode: None,
            uncertainty_generation: None,
        }))
    }
}

/// The mode the plan asks for; recorded for the human, never obeyed.
fn plan_requested_mode(args: Option<&Value>) -> Option<String> {
    args?.get("mode")?.as_str().map(str::to_owned)
}

/// The text the human is asked to approve a plan on.
fn plan_preview(args: Option<&Value>) -> Option<String> {
    let args = args?;
    let plan = args.get("plan")?.as_str()?;
    if plan.trim().is_empty() {
        return None;
    }
    Some(match args.get("mode").and_then(Value::as_str) {
        Some(mode) => format!(
            "{plan}

(requested mode: {mode})"
        ),
        None => plan.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentkit_core::{ToolDefinition, ToolLimits};
    use agentkit_host::ToolContributionContext;
    use serde_json::{Value, json};
    use tokio_util::sync::CancellationToken;

    fn context(capability: &str, name: &str, namespace: &str) -> ToolGuardContext {
        ToolGuardContext {
            contribution: ToolContributionContext {
                chat_id: Some("chat-1".into()),
                durable: None,
                run_id: "run-1".into(),
                scope_id: None,
                actor: Value::Null,
                actor_id: None,
                bindings: vec![],
                limits: ToolLimits::default(),
                cancellation: CancellationToken::new(),
                has_primary_binding: true,
                timeout_ms: 5_000,
            },
            namespace: namespace.to_owned(),
            arguments: None,
            tool: ToolDefinition {
                name: name.to_owned(),
                version: "1".into(),
                effect: "read".into(),
                capability: capability.to_owned(),
                description: String::new(),
                input_schema: json!({}),
                output_schema: None,
                timeout_ms: None,
            },
        }
    }

    async fn verdict(guard: &CodingPermissionGuard, capability: &str, name: &str) -> String {
        match guard
            .can_execute(&context(capability, name, CODING_NAMESPACE))
            .await
            .unwrap()
        {
            ToolGuardVerdict::Allowed => String::new(),
            ToolGuardVerdict::Refused(reason) => reason,
            ToolGuardVerdict::AwaitingHuman(ask) => ask.reason,
        }
    }

    fn guard(mode: CodingMode, role: Option<CodingRole>) -> CodingPermissionGuard {
        CodingPermissionGuard::new(CodingPermissionGuardOptions {
            mode,
            role,
            authority: None,
            plan_tool_names: None,
        })
    }

    #[tokio::test]
    async fn foreign_namespaces_get_no_opinion() {
        let guard = guard(CodingMode::ReadOnly, None);
        let verdict = match guard
            .can_execute(&context("fs.write", "write_file", "other"))
            .await
            .unwrap()
        {
            ToolGuardVerdict::Allowed => "allowed",
            ToolGuardVerdict::Refused(_) => "refused",
            ToolGuardVerdict::AwaitingHuman(_) => "awaiting_human",
        };
        assert_eq!(verdict, "allowed");
    }

    #[tokio::test]
    async fn unknown_capability_is_refused() {
        let guard = guard(CodingMode::FullAuto, None);
        assert_eq!(
            verdict(&guard, "nonsense", "x_tool").await,
            "unknown_capability: x_tool (nonsense)"
        );
    }

    #[tokio::test]
    async fn role_matrix() {
        // supervisor: read only, even in full_auto, and the ceiling is final.
        let supervisor = guard(CodingMode::FullAuto, Some(CodingRole::Supervisor));
        assert_eq!(verdict(&supervisor, "fs.read", "read_file").await, "");
        assert_eq!(
            verdict(&supervisor, "fs.write", "edit_file").await,
            "role_ceiling: supervisor may not use write tools (edit_file)"
        );
        assert_eq!(
            verdict(&supervisor, "process.exec", "run_command").await,
            "role_ceiling: supervisor may not use exec tools (run_command)"
        );
        // reviewer: no writes.
        let reviewer = guard(CodingMode::FullAuto, Some(CodingRole::Reviewer));
        assert_eq!(
            verdict(&reviewer, "fs.write", "edit_file").await,
            "role_ceiling: reviewer may not use write tools (edit_file)"
        );
        assert_eq!(verdict(&reviewer, "process.exec", "run_command").await, "");
    }

    #[tokio::test]
    async fn mode_matrix() {
        for (mode, cases) in [
            (
                CodingMode::Ask,
                [
                    ("fs.read", ""),
                    ("fs.write", "approval_required: edit_file"),
                    ("process.exec", "approval_required: run_command"),
                ],
            ),
            (
                CodingMode::AutoEdit,
                [
                    ("fs.read", ""),
                    ("fs.write", ""),
                    ("process.exec", "approval_required: run_command"),
                ],
            ),
            (
                CodingMode::FullAuto,
                [("fs.read", ""), ("fs.write", ""), ("process.exec", "")],
            ),
            (
                CodingMode::ReadOnly,
                [
                    ("fs.read", ""),
                    ("fs.write", "read_only: edit_file is not allowed"),
                    ("process.exec", "read_only: run_command is not allowed"),
                ],
            ),
        ] {
            let guard = guard(mode, None);
            for (capability, expected) in cases {
                let name = match capability {
                    "fs.write" => "edit_file",
                    "process.exec" => "run_command",
                    _ => "read_file",
                };
                assert_eq!(
                    verdict(&guard, capability, name).await,
                    expected,
                    "mode {} capability {}",
                    mode.as_str(),
                    capability
                );
            }
        }
    }

    #[tokio::test]
    async fn plan_mode_is_final_except_reads_and_plan_submission() {
        let guard = guard(CodingMode::Plan, None);
        assert_eq!(verdict(&guard, "fs.read", "read_file").await, "");
        assert!(
            verdict(&guard, "fs.write", "edit_file")
                .await
                .starts_with("plan mode:")
        );
        assert!(
            verdict(&guard, "process.exec", "run_command")
                .await
                .starts_with("plan mode:")
        );
        assert_eq!(
            verdict(&guard, SUBMIT_PLAN_CAPABILITY, SUBMIT_PLAN_TOOL_NAME).await,
            "plan_approval_required: submit_plan"
        );
    }

    struct FixedAuthority {
        mode: Option<CodingMode>,
        role: Option<CodingRole>,
    }

    #[async_trait]
    impl CodingPermissionAuthority for FixedAuthority {
        async fn mode_of(
            &self,
            _ctx: &PermissionContext,
        ) -> agentkit_host::HostResult<Option<CodingMode>> {
            Ok(self.mode)
        }
        async fn role_of(
            &self,
            _ctx: &PermissionContext,
        ) -> agentkit_host::HostResult<Option<CodingRole>> {
            Ok(self.role)
        }
    }

    struct FailingAuthority;

    #[async_trait]
    impl CodingPermissionAuthority for FailingAuthority {
        async fn role_of(
            &self,
            _ctx: &PermissionContext,
        ) -> agentkit_host::HostResult<Option<CodingRole>> {
            Err(agentkit_host::HostError::Execution(
                "session store unreachable".into(),
            ))
        }
    }

    #[tokio::test]
    async fn failing_authority_errors_rather_than_defaulting() {
        // TS: a rejecting `roleOf` throws out of the guard and host staging
        // FAILS CLOSED ("guard error" refusal). Defaulting to the configured
        // role here would let a broken authority silently grant `implementer`
        // to a supervisor session.
        let guard = CodingPermissionGuard::new(CodingPermissionGuardOptions {
            mode: CodingMode::FullAuto,
            role: None,
            authority: Some(Arc::new(FailingAuthority)),
            plan_tool_names: None,
        });
        assert!(
            guard
                .can_execute(&context("fs.write", "write_file", CODING_NAMESPACE))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn authority_overrides_configuration() {
        let guard = CodingPermissionGuard::new(CodingPermissionGuardOptions {
            mode: CodingMode::FullAuto,
            role: None,
            authority: Some(Arc::new(FixedAuthority {
                mode: Some(CodingMode::ReadOnly),
                role: Some(CodingRole::Supervisor),
            })),
            plan_tool_names: None,
        });
        assert_eq!(
            verdict(&guard, "fs.write", "edit_file").await,
            "role_ceiling: supervisor may not use write tools (edit_file)"
        );
    }
}
