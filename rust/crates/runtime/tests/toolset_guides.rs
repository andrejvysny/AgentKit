//! The AgentKit toolsets (T4.1, TU-27): ids, cards, guides, and guide
//! placeholders rendered through the common dialect.
//!
//! This crate is where the layering lets one test reach every toolset owner.

use std::collections::{BTreeMap, BTreeSet};

use agentkit_agents::{SUBAGENT_TOOL_NAMES, delegation_toolsets};
use agentkit_core::{Toolset, common_dialect, guide_placeholders, render_guide};
use agentkit_lsp::lsp_toolsets;
use agentkit_toolpacks_coding::{CODING_TOOL_NAMES, coding_toolsets};

const LSP_TOOL_NAMES: [&str; 6] = [
    "lsp_definition",
    "lsp_references",
    "lsp_hover",
    "lsp_symbols",
    "lsp_implementation",
    "lsp_diagnostics",
];

fn all_toolsets() -> Vec<Toolset> {
    coding_toolsets()
        .into_iter()
        .chain(lsp_toolsets())
        .chain(delegation_toolsets())
        .collect()
}

#[test]
fn the_agentkit_toolsets_are_files_shell_git_plan_code_intel_and_delegation() {
    let ids: Vec<String> = all_toolsets().into_iter().map(|set| set.id).collect();
    assert_eq!(
        ids,
        ["files", "shell", "git", "plan", "code_intel", "delegation"]
    );
}

#[test]
fn every_static_tool_is_in_exactly_one_toolset() {
    let mut seen: BTreeMap<String, String> = BTreeMap::new();
    for set in all_toolsets() {
        for tool in &set.tools {
            let earlier = seen.insert(tool.clone(), set.id.clone());
            assert!(earlier.is_none(), "{tool} is in two toolsets");
        }
    }
    let expected: BTreeSet<String> = CODING_TOOL_NAMES
        .iter()
        .chain(SUBAGENT_TOOL_NAMES.iter())
        .chain(LSP_TOOL_NAMES.iter())
        .map(|name| (*name).to_owned())
        .collect();
    assert_eq!(seen.keys().cloned().collect::<BTreeSet<_>>(), expected);
}

#[test]
fn a_card_is_one_short_line_and_a_guide_is_versioned_and_small() {
    for set in all_toolsets() {
        assert!(
            !set.card.is_empty() && set.card.len() <= 120 && !set.card.contains('\n'),
            "{}: card",
            set.id
        );
        let guide = set.guide.as_ref().unwrap_or_else(|| panic!("{} has a guide", set.id));
        assert!(!guide.version.is_empty(), "{}: version", set.id);
        assert!(
            guide.estimated_tokens() <= 600,
            "{}: {} tokens",
            set.id,
            guide.estimated_tokens()
        );
        for part in ["Example 1", "Example 2", "Common mistakes", "Which "] {
            assert!(guide.text.contains(part), "{}: missing {part}", set.id);
        }
    }
}

#[test]
fn guides_name_only_known_tools_and_render_the_dialect_names() {
    let known: BTreeSet<String> = CODING_TOOL_NAMES
        .iter()
        .chain(SUBAGENT_TOOL_NAMES.iter())
        .chain(LSP_TOOL_NAMES.iter())
        .map(|name| (*name).to_owned())
        .collect();
    let dialect = common_dialect();
    let shown: BTreeMap<String, String> = known
        .iter()
        .map(|canonical| {
            let wires = dialect.wire_names_of(canonical);
            let text = match wires.as_slice() {
                [] => canonical.clone(),
                [only] => only.clone(),
                [rest @ .., last] => format!("{} or {last}", rest.join(", ")),
            };
            (canonical.clone(), text)
        })
        .collect();
    for set in all_toolsets() {
        let guide = set.guide.expect("guide");
        for id in guide_placeholders(&guide.text) {
            assert!(known.contains(&id), "{}: placeholder {id} is not a tool", set.id);
        }
        let text = render_guide(&guide.text, &shown);
        assert!(!text.contains("{{"), "{}: placeholder left", set.id);
        // The model reads the names it was offered, not canonical ids.
        for renamed in ["edit_file", "code_search", "run_command", "git_inspect"] {
            assert!(
                !text.contains(renamed),
                "{}: still names {renamed} after rendering",
                set.id
            );
        }
        if set.id == "files" {
            assert!(text.contains("grep_search") && text.contains("edit"));
        }
        if set.id == "git" {
            assert!(text.contains("git_status, git_diff, git_log or git_show"));
        }
    }
}
