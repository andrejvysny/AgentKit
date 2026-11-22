//! Spawn-time authority intersection (roadmap 4.4, spec "Authority"): a
//! child's effective tool set is the role allowlist intersected with what
//! its parent can use, narrowed by the execution mode the child will run in.
//! The result is frozen into the role snapshot, so nothing the parent or the
//! configuration gains later widens a live child.

use super::config::RoleSpec;
use super::snapshot::EffectiveRole;
use super::tools::SUBAGENT_TOOL_NAMES;
use crate::ADVISOR_TOOL_NAMES;
use agentkit_core::permission::{CodingMode, ToolClass, classify_capability};
use agentkit_core::{ToolDefinition, is_meta_tool};
use std::collections::BTreeSet;

/// Memory tools a child never gets: it reads memory (`memory_search`,
/// `memory_get`) but proposals and deletions stay with the parent's human.
/// [`is_child_denied`] widens this by capability and effect.
pub(super) const CHILD_DENIED_TOOLS: [&str; 2] = ["memory_propose", "memory_forget"];

/// The contributor namespace of the delegation tools.
pub(super) const SUBAGENT_NAMESPACE: &str = "subagent";

/// An effective-set entry meaning "no parent bound": recorded for a parent
/// that is not a session (no recorded manifest to intersect with), so the
/// role allowlist alone applies. Written at spawn and covered by the digest.
pub(super) const UNBOUNDED: &str = "*";

/// A tool's identity: the contributor namespace and the name. A bare name
/// is never an identity, because two packs may contribute the same one.
pub(super) fn tool_key(namespace: &str, name: &str) -> String {
    format!("{namespace}:{name}")
}

/// A tool the parent session recorded, with the namespace it was
/// contributed under.
pub(super) struct ParentTool {
    pub namespace: String,
    pub definition: ToolDefinition,
}

impl ParentTool {
    fn key(&self) -> String {
        tool_key(&self.namespace, &self.definition.name)
    }
}

/// The execution mode a child runs in: never more permissive than the
/// parent's, and a read-only role (or a session with no workspace to write
/// to) pins `read_only`. A parent mode that is missing or unreadable also
/// pins `read_only`: the child never guesses a more permissive default.
pub(super) fn child_mode(
    parent_mode: Option<&str>,
    role_read_only: bool,
    has_workspace: bool,
) -> Option<String> {
    let read_only = || Some(CodingMode::ReadOnly.as_str().to_owned());
    if role_read_only || !has_workspace {
        return read_only();
    }
    match parent_mode.and_then(CodingMode::parse) {
        Some(mode) => Some(mode.as_str().to_owned()),
        None => read_only(),
    }
}

/// Whether a mode refuses every write and command (so the tools that need
/// them are not part of the effective set at all). Only the modes that let
/// a write or command run say no; a missing or unknown mode counts as
/// read-only.
pub(super) fn mode_is_read_only(mode: Option<&str>) -> bool {
    !matches!(
        mode.and_then(CodingMode::parse),
        Some(CodingMode::Ask | CodingMode::AutoEdit | CodingMode::FullAuto)
    )
}

/// True when the tool only reads. The stricter of the two signals wins: a
/// recognised capability must be a read, and the declared effect must be
/// `read` too, so `write`, `execute`, `external` and anything unknown fail
/// closed.
pub(super) fn is_read_tool(definition: &ToolDefinition) -> bool {
    let capability_reads = classify_capability(&definition.capability)
        .is_none_or(|class| matches!(class, ToolClass::Read | ToolClass::Net));
    capability_reads && definition.effect == "read"
}

/// True when the tool changes the workspace: a write or exec capability
/// class, or a write or execute effect. Memory tools write to the memory
/// store, not the workspace.
pub(super) fn mutates_workspace(namespace: &str, definition: &ToolDefinition) -> bool {
    if is_memory_tool(namespace, definition) {
        return false;
    }
    matches!(
        classify_capability(&definition.capability),
        Some(ToolClass::Write | ToolClass::Exec)
    ) || matches!(definition.effect.as_str(), "write" | "execute")
}

fn is_memory_tool(namespace: &str, definition: &ToolDefinition) -> bool {
    namespace == "memory" || definition.capability.starts_with("memory.")
}

/// Tools a child never runs: the named memory writes, any tool with the
/// memory write capability or a non-read effect in the memory namespace, and
/// the advisor tools (the advisor runs outside the child's token budget).
pub(super) fn is_child_denied(namespace: &str, definition: &ToolDefinition) -> bool {
    CHILD_DENIED_TOOLS.contains(&definition.name.as_str())
        || ADVISOR_TOOL_NAMES.contains(&definition.name.as_str())
        || definition.capability == "memory.write"
        || (is_memory_tool(namespace, definition) && definition.effect != "read")
}

/// Whether one allowlist admits a tool. `*` admits all; a qualified entry
/// must match the namespace AND the name; a bare entry names a NAME, never
/// a namespace, so it admits read tools only (a same-named effect-bearing
/// tool from another pack must be granted by its qualified name).
pub(super) fn entry_admits(
    allowed: &[String],
    namespace: &str,
    definition: &ToolDefinition,
) -> bool {
    allowed.iter().any(|entry| match entry.split_once(':') {
        Some((entry_ns, name)) => entry_ns == namespace && name == definition.name,
        None => entry == "*" || (entry == &definition.name && is_read_tool(definition)),
    })
}

fn is_spawn_tool(namespace: &str, definition: &ToolDefinition) -> bool {
    namespace == SUBAGENT_NAMESPACE && SUBAGENT_TOOL_NAMES.contains(&definition.name.as_str())
}

fn allowlist_admits(allowed: &[String], can_spawn: bool, tool: &ParentTool) -> bool {
    (can_spawn && is_spawn_tool(&tool.namespace, &tool.definition))
        || entry_admits(allowed, &tool.namespace, &tool.definition)
}

/// What the spawning session may use, as far as spawn can tell.
pub(super) enum ParentAuthority<'a> {
    /// A root session: its guard chain governs it; the manifest is the set.
    Root,
    /// A child session: its own frozen authority bounds the set.
    Child(&'a EffectiveRole),
}

/// The child's effective tool identities (`namespace:name`), sorted.
/// `tools` is every tool the parent session recorded. `reads_only` drops
/// every tool that can write or run commands (read-only mode, or no
/// workspace).
pub(super) fn effective_tools(
    role: &RoleSpec,
    tools: &[ParentTool],
    parent: &ParentAuthority<'_>,
    reads_only: bool,
) -> Vec<String> {
    let keys: BTreeSet<String> = tools
        .iter()
        .filter(|tool| !is_meta_tool(&tool.definition.name))
        .filter(|tool| !is_child_denied(&tool.namespace, &tool.definition))
        .filter(|tool| !reads_only || is_read_tool(&tool.definition))
        .filter(|tool| allowlist_admits(&role.allowed_tools, role.can_spawn, tool))
        .filter(|tool| parent_admits(parent, tool))
        .map(ParentTool::key)
        .collect();
    keys.into_iter().collect()
}

fn parent_admits(parent: &ParentAuthority<'_>, tool: &ParentTool) -> bool {
    let ParentAuthority::Child(role) = parent else {
        return true;
    };
    if role.read_tools_only && !is_read_tool(&tool.definition) {
        return false;
    }
    let frozen = match &role.effective_tools {
        Some(set) => {
            let key = tool.key();
            set.iter().any(|entry| entry == UNBOUNDED || entry == &key)
        }
        None => true,
    };
    frozen
        && role
            .tool_sets
            .iter()
            .all(|set| allowlist_admits(set, role.can_spawn, tool))
}

/// A child that can write or run commands is a writer: it must hold the
/// exclusive workspace lease for as long as it runs.
pub(super) fn is_writer(tools: &[ParentTool], effective: &[String]) -> bool {
    tools
        .iter()
        .filter(|tool| effective.contains(&tool.key()))
        .any(|tool| !is_read_tool(&tool.definition))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool(name: &str, capability: &str, effect: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.into(),
            version: "1".into(),
            effect: effect.into(),
            capability: capability.into(),
            description: String::new(),
            input_schema: json!({"type": "object"}),
            output_schema: None,
            timeout_ms: None,
            examples: vec![],
        }
    }

    fn parent_tool(namespace: &str, definition: ToolDefinition) -> ParentTool {
        ParentTool {
            namespace: namespace.into(),
            definition,
        }
    }

    fn manifest() -> Vec<ParentTool> {
        vec![
            parent_tool("coding", tool("read_file", "fs.read", "read")),
            parent_tool("coding", tool("write_file", "fs.write", "write")),
            parent_tool("coding", tool("run_command", "process.exec", "execute")),
            parent_tool("net", tool("web_fetch", "net.fetch", "external")),
            parent_tool("memory", tool("memory_search", "memory.read", "read")),
            parent_tool("memory", tool("memory_propose", "memory.write", "write")),
            parent_tool("memory", tool("memory_forget", "memory.write", "write")),
            parent_tool("subagent", tool("subagent_spawn", "orchestration", "read")),
            parent_tool("meta", tool("tool_search", "meta", "read")),
        ]
    }

    fn role(allowed: &[&str], can_spawn: bool) -> RoleSpec {
        RoleSpec {
            id: "r".into(),
            allowed_tools: allowed.iter().map(|tool| (*tool).to_owned()).collect(),
            read_only: false,
            can_spawn,
            ..Default::default()
        }
    }

    fn names(role: &RoleSpec, parent: &ParentAuthority<'_>, reads_only: bool) -> Vec<String> {
        effective_tools(role, &manifest(), parent, reads_only)
    }

    fn child_of(effective: Option<&[&str]>) -> EffectiveRole {
        EffectiveRole {
            id: "p".into(),
            tool_sets: vec![vec!["*".into()]],
            read_only: false,
            can_spawn: true,
            effective_tools: effective.map(|set| set.iter().map(|key| (*key).to_owned()).collect()),
            read_tools_only: false,
        }
    }

    #[test]
    fn star_expands_to_the_parent_set_minus_memory_writes_and_meta_tools() {
        let all = names(&role(&["*"], false), &ParentAuthority::Root, false);
        assert!(all.contains(&"coding:write_file".to_owned()));
        assert!(all.contains(&"memory:memory_search".to_owned()));
        for denied in CHILD_DENIED_TOOLS {
            assert!(!all.iter().any(|key| key.ends_with(denied)));
        }
        assert!(!all.iter().any(|key| key.ends_with("tool_search")));
    }

    #[test]
    fn read_only_mode_drops_every_non_read_tool() {
        let reads = names(&role(&["*"], true), &ParentAuthority::Root, true);
        assert_eq!(
            reads,
            [
                "coding:read_file",
                "memory:memory_search",
                "subagent:subagent_spawn"
            ]
            .map(str::to_owned)
        );
        assert!(!is_writer(&manifest(), &reads));
    }

    #[test]
    fn a_writing_set_is_a_writer() {
        let set = names(
            &role(&["coding:write_file"], false),
            &ParentAuthority::Root,
            false,
        );
        assert_eq!(set, vec!["coding:write_file".to_owned()]);
        assert!(is_writer(&manifest(), &set));
        let exec = names(
            &role(&["coding:run_command"], false),
            &ParentAuthority::Root,
            false,
        );
        assert!(is_writer(&manifest(), &exec));
        let external = names(
            &role(&["net:web_fetch"], false),
            &ParentAuthority::Root,
            false,
        );
        assert!(is_writer(&manifest(), &external));
    }

    #[test]
    fn the_stricter_of_capability_and_effect_makes_a_writer() {
        // A write effect behind a read capability, and the reverse.
        assert!(!is_read_tool(&tool("a", "fs.read", "write")));
        assert!(!is_read_tool(&tool("a", "fs.write", "read")));
        assert!(!is_read_tool(&tool("a", "unknown.cap", "execute")));
        assert!(is_read_tool(&tool("a", "unknown.cap", "read")));
        let sneaky = vec![parent_tool("mcp", tool("peek", "fs.read", "write"))];
        let set = effective_tools(
            &role(&["mcp:peek"], false),
            &sneaky,
            &ParentAuthority::Root,
            false,
        );
        assert!(is_writer(&sneaky, &set));
    }

    #[test]
    fn a_bare_entry_never_admits_an_effect_bearing_tool() {
        let set = names(
            &role(&["write_file", "read_file"], false),
            &ParentAuthority::Root,
            false,
        );
        assert_eq!(set, vec!["coding:read_file".to_owned()]);
    }

    #[test]
    fn a_qualified_entry_needs_the_same_namespace() {
        let tools = vec![
            parent_tool("coding", tool("read_file", "fs.read", "read")),
            parent_tool("mcp", tool("read_file", "fs.read", "read")),
        ];
        let set = effective_tools(
            &role(&["coding:read_file"], false),
            &tools,
            &ParentAuthority::Root,
            false,
        );
        assert_eq!(set, vec!["coding:read_file".to_owned()]);
    }

    #[test]
    fn a_child_parent_bounds_the_grandchild_by_namespace_and_name() {
        let parent = child_of(Some(&["coding:read_file"]));
        let set = names(&role(&["*"], true), &ParentAuthority::Child(&parent), false);
        assert_eq!(set, vec!["coding:read_file".to_owned()]);
        // The same name from another namespace is not the parent's tool.
        let other = vec![parent_tool("mcp", tool("read_file", "fs.read", "read"))];
        let set = effective_tools(
            &role(&["*"], true),
            &other,
            &ParentAuthority::Child(&parent),
            false,
        );
        assert!(set.is_empty());

        let legacy = EffectiveRole {
            effective_tools: None,
            tool_sets: vec![vec!["coding:read_file".into()]],
            can_spawn: false,
            ..child_of(None)
        };
        let set = names(&role(&["*"], true), &ParentAuthority::Child(&legacy), false);
        assert_eq!(set, vec!["coding:read_file".to_owned()]);

        let pre_authority = EffectiveRole {
            read_tools_only: true,
            ..child_of(None)
        };
        let set = names(
            &role(&["*"], false),
            &ParentAuthority::Child(&pre_authority),
            false,
        );
        assert!(!set.contains(&"coding:write_file".to_owned()));
        assert!(set.contains(&"coding:read_file".to_owned()));
    }

    #[test]
    fn memory_write_capability_or_effect_is_denied_whatever_the_name() {
        assert!(is_child_denied(
            "memory",
            &tool("memory_propose", "x", "read")
        ));
        assert!(is_child_denied(
            "memory",
            &tool("remember", "memory.write", "write")
        ));
        assert!(is_child_denied(
            "custom",
            &tool("note", "memory.write", "read")
        ));
        assert!(is_child_denied(
            "memory",
            &tool("rewrite", "memory.admin", "write")
        ));
        assert!(!is_child_denied(
            "memory",
            &tool("memory_search", "memory.read", "read")
        ));
        let tools = vec![parent_tool(
            "memory",
            tool("remember", "memory.write", "write"),
        )];
        let set = effective_tools(&role(&["*"], false), &tools, &ParentAuthority::Root, false);
        assert!(set.is_empty());
    }

    #[test]
    fn workspace_mutation_follows_class_and_effect_not_names() {
        assert!(mutates_workspace(
            "mcp",
            &tool("anything", "fs.write", "read")
        ));
        assert!(mutates_workspace(
            "mcp",
            &tool("anything", "process.exec", "read")
        ));
        assert!(mutates_workspace("mcp", &tool("anything", "x", "write")));
        assert!(mutates_workspace(
            "coding",
            &tool("run_tests", "x", "execute")
        ));
        assert!(!mutates_workspace(
            "coding",
            &tool("read_file", "fs.read", "read")
        ));
        assert!(!mutates_workspace(
            "memory",
            &tool("memory_propose", "memory.write", "write")
        ));
    }

    #[test]
    fn mode_is_never_more_permissive_than_the_parent() {
        assert_eq!(
            child_mode(Some("full_auto"), true, true).as_deref(),
            Some("read_only")
        );
        assert_eq!(child_mode(Some("ask"), false, true).as_deref(), Some("ask"));
        assert_eq!(
            child_mode(Some("full_auto"), false, false).as_deref(),
            Some("read_only")
        );
        assert_eq!(
            child_mode(None, false, true).as_deref(),
            Some("read_only"),
            "an unknown parent mode is read-only"
        );
        assert_eq!(
            child_mode(Some("bogus"), false, true).as_deref(),
            Some("read_only")
        );
        assert!(mode_is_read_only(Some("plan")));
        assert!(mode_is_read_only(Some("read_only")));
        assert!(!mode_is_read_only(Some("ask")));
        assert!(mode_is_read_only(None));
        assert!(mode_is_read_only(Some("bogus")));
    }
}
