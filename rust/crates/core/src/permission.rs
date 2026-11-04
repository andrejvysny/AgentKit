//! The ONE portable permission decision shared by the coding pack's guard
//! and every binding-side tool guard.
//!
//! This module is deliberately dependency-free and platform-neutral: it
//! carries the mode×class matrix, the capability→class classification, and
//! the role ceilings, so a JS-contributed tool and a native coding tool are
//! judged by the SAME code. Namespaces, verdict wording, metadata adapters
//! (declared effects, plan-tool name lists), and platform execution live in
//! the consuming crates — a divergence between two guards is a bug, not a
//! per-surface policy choice.

/// What a tool can DO, the only axis the permission matrix judges.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolClass {
    Read,
    Write,
    /// Command execution. `auto_edit` does NOT cover it: a command is not
    /// an edit, and only `full_auto` (or a per-call approval) runs one.
    Exec,
}

impl ToolClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Exec => "exec",
        }
    }
}

/// Capability strings the classifier recognises. A tool declaring one of
/// these is judged by its CAPABILITY, never by a surface-supplied effect
/// string; unknown capabilities fall back to surface metadata (or refusal,
/// at the surface's discretion).
pub const FS_READ_CAPABILITY: &str = "fs.read";
pub const GIT_READ_CAPABILITY: &str = "git.read";
pub const FS_WRITE_CAPABILITY: &str = "fs.write";
pub const PROCESS_EXEC_CAPABILITY: &str = "process.exec";
/// The plan-submission capability: the tool's whole effect is the human
/// decision, so it classifies as Read.
pub const PLAN_CAPABILITY: &str = "plan";

/// Capability-based classification, the coding pack's table; `None` means
/// "not a recognized capability" — the caller decides between refusal
/// (native tools must fail closed) and effect fallback (JS tools).
pub fn classify_capability(capability: &str) -> Option<ToolClass> {
    Some(match capability {
        FS_READ_CAPABILITY | GIT_READ_CAPABILITY => ToolClass::Read,
        FS_WRITE_CAPABILITY => ToolClass::Write,
        PROCESS_EXEC_CAPABILITY => ToolClass::Exec,
        PLAN_CAPABILITY => ToolClass::Read,
        _ => return None,
    })
}

/// The execution modes. Session state, not process configuration: only the
/// human (directly or through an approved plan) moves a session between
/// them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodingMode {
    ReadOnly,
    Ask,
    AutoEdit,
    FullAuto,
    /// Reads only. Writes and commands are refused FINAL (no per-call
    /// approval): the only escalation is a plan submission the human
    /// approves, recorded as a durable grant before anything may change.
    Plan,
}

impl CodingMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::Ask => "ask",
            Self::AutoEdit => "auto_edit",
            Self::FullAuto => "full_auto",
            Self::Plan => "plan",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "read_only" => Self::ReadOnly,
            "ask" => Self::Ask,
            "auto_edit" => Self::AutoEdit,
            "full_auto" => Self::FullAuto,
            "plan" => Self::Plan,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodingRole {
    Implementer,
    Reviewer,
    Supervisor,
}

impl CodingRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Implementer => "implementer",
            Self::Reviewer => "reviewer",
            Self::Supervisor => "supervisor",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "implementer" => Self::Implementer,
            "reviewer" => Self::Reviewer,
            "supervisor" => Self::Supervisor,
            _ => return None,
        })
    }
}

/// Role ceilings (adapted from OneMind `tool-authority.ts`, MIT). The
/// ceiling wins over the mode and NO mode lifts it: a supervisor stays
/// read-only even in `full_auto`, because the ceiling refusal carries no
/// approval a human could answer.
pub fn role_ceiling_allows(role: CodingRole, tool_class: ToolClass) -> bool {
    match role {
        CodingRole::Supervisor => tool_class == ToolClass::Read,
        CodingRole::Reviewer => matches!(tool_class, ToolClass::Read | ToolClass::Exec),
        CodingRole::Implementer => true,
    }
}

/// What the model is told when plan mode blocks it: the way out, by name.
pub const PLAN_MODE_BLOCKED_HINT: &str =
    "plan mode: propose the change with submit_plan and wait for the user to approve it.";

/// The structured decision of the shared matrix. Callers render verdict
/// WORDING around it (byte-stable per surface); they must not re-branch on
/// mode/class themselves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionDecision {
    Allowed,
    /// A plan submission in plan mode: the human decides on the plan text.
    PlanApprovalRequired,
    /// A final refusal — no answer can reopen it.
    BlockedFinal(BlockedFinal),
    /// A per-call human approval; answering it allows THIS call.
    ApprovalRequired,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockedFinal {
    /// Plan mode blocks writes and commands outright; a per-call approval
    /// in plan mode would let the model walk through the door one small
    /// edit at a time. Carries the class for the model-facing hint.
    PlanMode {
        tool_class: ToolClass,
    },
    ReadOnly,
}

/// The ONE matrix. `is_plan_tool` means the tool's NAME is in the
/// session/host plan-tool list — checked BEFORE the class axes, so a
/// plan-named tool never slips through on a declared read effect.
pub fn decide(mode: CodingMode, tool_class: ToolClass, is_plan_tool: bool) -> PermissionDecision {
    match mode {
        CodingMode::Plan => {
            if is_plan_tool {
                return PermissionDecision::PlanApprovalRequired;
            }
            match tool_class {
                ToolClass::Read => PermissionDecision::Allowed,
                tool_class => {
                    PermissionDecision::BlockedFinal(BlockedFinal::PlanMode { tool_class })
                }
            }
        }
        CodingMode::ReadOnly => match tool_class {
            ToolClass::Read => PermissionDecision::Allowed,
            _ => PermissionDecision::BlockedFinal(BlockedFinal::ReadOnly),
        },
        CodingMode::Ask => match tool_class {
            ToolClass::Read => PermissionDecision::Allowed,
            _ => PermissionDecision::ApprovalRequired,
        },
        CodingMode::AutoEdit => match tool_class {
            ToolClass::Read | ToolClass::Write => PermissionDecision::Allowed,
            // Command execution is NOT an edit: auto_edit asks.
            ToolClass::Exec => PermissionDecision::ApprovalRequired,
        },
        CodingMode::FullAuto => PermissionDecision::Allowed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_uses_capability_not_effect() {
        assert_eq!(classify_capability("process.exec"), Some(ToolClass::Exec));
        assert_eq!(classify_capability("fs.write"), Some(ToolClass::Write));
        assert_eq!(classify_capability("plan"), Some(ToolClass::Read));
        assert_eq!(classify_capability("git.read"), Some(ToolClass::Read));
        assert_eq!(classify_capability("whatever"), None);
    }

    #[test]
    fn auto_edit_asks_for_commands_allows_writes() {
        // The binding divergence the assessment caught: auto_edit allowed
        // a process.exec tool because the caller branched on effect alone.
        assert_eq!(
            decide(CodingMode::AutoEdit, ToolClass::Exec, false),
            PermissionDecision::ApprovalRequired
        );
        assert_eq!(
            decide(CodingMode::AutoEdit, ToolClass::Write, false),
            PermissionDecision::Allowed
        );
        assert_eq!(
            decide(CodingMode::AutoEdit, ToolClass::Read, false),
            PermissionDecision::Allowed
        );
    }

    #[test]
    fn plan_mode_blocks_writes_and_commands_but_asks_for_plan_tools() {
        assert_eq!(
            decide(CodingMode::Plan, ToolClass::Write, false),
            PermissionDecision::BlockedFinal(BlockedFinal::PlanMode {
                tool_class: ToolClass::Write
            })
        );
        assert_eq!(
            decide(CodingMode::Plan, ToolClass::Exec, false),
            PermissionDecision::BlockedFinal(BlockedFinal::PlanMode {
                tool_class: ToolClass::Exec
            })
        );
        assert_eq!(
            decide(CodingMode::Plan, ToolClass::Read, false),
            PermissionDecision::Allowed
        );
        // A plan-named tool with a declared READ effect still asks — the
        // name check precedes the class axes.
        assert_eq!(
            decide(CodingMode::Plan, ToolClass::Read, true),
            PermissionDecision::PlanApprovalRequired
        );
        // A plan-named tool that declares WRITE is still the plan
        // submission the human decides on, not a final block.
        assert_eq!(
            decide(CodingMode::Plan, ToolClass::Write, true),
            PermissionDecision::PlanApprovalRequired
        );
    }

    #[test]
    fn ask_and_read_only_and_full_auto() {
        assert_eq!(
            decide(CodingMode::Ask, ToolClass::Write, false),
            PermissionDecision::ApprovalRequired
        );
        assert_eq!(
            decide(CodingMode::Ask, ToolClass::Read, false),
            PermissionDecision::Allowed
        );
        assert_eq!(
            decide(CodingMode::ReadOnly, ToolClass::Write, false),
            PermissionDecision::BlockedFinal(BlockedFinal::ReadOnly)
        );
        assert_eq!(
            decide(CodingMode::FullAuto, ToolClass::Exec, false),
            PermissionDecision::Allowed
        );
    }

    #[test]
    fn role_ceiling_wins_regardless_of_mode() {
        assert!(!role_ceiling_allows(
            CodingRole::Supervisor,
            ToolClass::Write
        ));
        assert!(!role_ceiling_allows(
            CodingRole::Supervisor,
            ToolClass::Exec
        ));
        assert!(role_ceiling_allows(CodingRole::Reviewer, ToolClass::Exec));
        assert!(!role_ceiling_allows(CodingRole::Reviewer, ToolClass::Write));
        assert!(role_ceiling_allows(
            CodingRole::Implementer,
            ToolClass::Exec
        ));
    }

    #[test]
    fn mode_strings_round_trip() {
        for mode in [
            CodingMode::ReadOnly,
            CodingMode::Ask,
            CodingMode::AutoEdit,
            CodingMode::FullAuto,
            CodingMode::Plan,
        ] {
            assert_eq!(CodingMode::parse(mode.as_str()), Some(mode));
        }
        assert_eq!(CodingMode::parse("yolo"), None);
    }
}
