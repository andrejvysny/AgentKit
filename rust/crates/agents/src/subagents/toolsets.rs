//! The subagent tools as the `delegation` toolset (T4.1, ADR 0033).

use super::tools::SUBAGENT_TOOL_NAMES;
use agentkit_core::Toolset;

pub const DELEGATION_TOOLSET: &str = "delegation";

pub fn delegation_toolsets() -> Vec<Toolset> {
    vec![
        Toolset::new(
            DELEGATION_TOOLSET,
            "Delegation",
            "Hand work to child agents, wait for them, and steer or stop them.",
        )
        .with_guide(include_str!("../../guides/delegation.md"))
        .with_tools(&SUBAGENT_TOOL_NAMES),
    ]
}
