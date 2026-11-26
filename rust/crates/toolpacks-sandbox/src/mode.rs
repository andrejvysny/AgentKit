//! The five permission modes as they apply inside a sandbox (spec
//! "Permission modes", D-13, D-29). The sandbox is the safety boundary, so
//! the table is narrower than the coding matrix: commands and writes run
//! without a prompt in Ask and Edit, and only boundary crossings ask.

use std::sync::Arc;

use agentkit_core::permission::CodingMode;
use agentkit_governance::{
    CodingPermissionAuthority, GovernanceResult, PermissionContext, ToolContributionContext,
};

use crate::defs::ToolKind;

/// A run's permission mode in the sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxMode {
    /// Reads only; commands, writes and exports are refused.
    ReadOnly,
    /// Reads only; changes are refused final (no per-call approval).
    Plan,
    /// Commands and writes run; exports and new hosts ask. A chat turn
    /// runs in this mode.
    AskBeforeChanges,
    /// Same table as `AskBeforeChanges` inside a sandbox.
    EditAutomatically,
    /// Exports run without approval (D-29); new hosts still ask.
    FullAccess,
}

impl SandboxMode {
    /// The mode of a chat turn: Chat has no mode selector.
    pub const CHAT: SandboxMode = SandboxMode::AskBeforeChanges;

    /// The sandbox row of a coding mode.
    pub fn from_coding_mode(mode: CodingMode) -> Self {
        match mode {
            CodingMode::ReadOnly => SandboxMode::ReadOnly,
            CodingMode::Plan => SandboxMode::Plan,
            CodingMode::Ask => SandboxMode::AskBeforeChanges,
            CodingMode::AutoEdit => SandboxMode::EditAutomatically,
            CodingMode::FullAuto => SandboxMode::FullAccess,
        }
    }

    /// A coding mode's wire name (`read_only`, `plan`, `ask`, `auto_edit`,
    /// `full_auto`); anything else is the most restrictive mode.
    pub fn parse(text: &str) -> Self {
        CodingMode::parse(text).map_or(SandboxMode::ReadOnly, Self::from_coding_mode)
    }

    /// True when commands and writes may change the sandbox.
    pub fn may_change(self) -> bool {
        !matches!(self, SandboxMode::ReadOnly | SandboxMode::Plan)
    }

    /// True when an export needs the owner's approval of its content.
    pub fn export_needs_approval(self) -> bool {
        self != SandboxMode::FullAccess
    }

    /// The final refusal, as `(code, message)`, of a tool this mode does not
    /// allow; `None` when allowed (an approval may still be needed).
    pub(crate) fn refusal(self, kind: ToolKind) -> Option<(&'static str, String)> {
        if !kind.changes() || self.may_change() {
            return None;
        }
        let name = kind.name();
        Some(match self {
            SandboxMode::Plan => (
                "plan_mode",
                format!(
                    "{name} changes the sandbox and this run is in plan mode; describe the change in your plan and wait for the user to approve it"
                ),
            ),
            _ => (
                "read_only",
                format!("{name} changes the sandbox and this run is read-only"),
            ),
        })
    }
}

impl From<CodingMode> for SandboxMode {
    fn from(mode: CodingMode) -> Self {
        Self::from_coding_mode(mode)
    }
}

/// The mode a call runs under: the host authority's live answer for the
/// session, else the mode pinned with the binding. An authority that cannot
/// answer fails the call closed.
pub(crate) async fn effective_mode(
    authority: Option<&Arc<dyn CodingPermissionAuthority>>,
    contribution: &ToolContributionContext,
    pinned: SandboxMode,
) -> GovernanceResult<SandboxMode> {
    let Some(authority) = authority else {
        return Ok(pinned);
    };
    let session = PermissionContext {
        chat_id: contribution.chat_id.clone(),
        run_id: Some(contribution.run_id.clone()),
    };
    Ok(authority
        .mode_of(&session)
        .await?
        .map_or(pinned, SandboxMode::from_coding_mode))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_coding_mode_has_its_row_and_unknown_text_is_read_only() {
        let rows = [
            ("read_only", SandboxMode::ReadOnly),
            ("plan", SandboxMode::Plan),
            ("ask", SandboxMode::AskBeforeChanges),
            ("auto_edit", SandboxMode::EditAutomatically),
            ("full_auto", SandboxMode::FullAccess),
            ("yolo", SandboxMode::ReadOnly),
            ("", SandboxMode::ReadOnly),
        ];
        for (text, mode) in rows {
            assert_eq!(SandboxMode::parse(text), mode, "{text}");
        }
        assert_eq!(SandboxMode::CHAT, SandboxMode::AskBeforeChanges);
        assert!(!SandboxMode::Plan.may_change());
        assert!(SandboxMode::EditAutomatically.export_needs_approval());
        assert!(!SandboxMode::FullAccess.export_needs_approval());
    }
}
