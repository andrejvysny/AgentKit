use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Every subagent role this build knows, in canonical order.
pub const BUILTIN_ROLES: [&str; 5] = [
    "general",
    "researcher",
    "explorer",
    "implementer",
    "reviewer",
];

/// One delegated-workforce role: a behavior ceiling plus an optional model
/// override. `allowed_tools` entries are `namespace:name` or bare `name`;
/// a bare entry matches the name in ANY namespace. `"*"` allows everything
/// the host's own guard chain would allow the parent anyway.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RoleSpec {
    pub id: String,
    pub description: String,
    /// Prepended to the spawn prompt as the child's role briefing.
    pub prompt: String,
    /// Tool allowlist; empty vector means "deny all tools".
    pub allowed_tools: Vec<String>,
    /// True when the role may not mutate the workspace or run commands.
    /// Read-only roles are pinned to the session `read_only` execution
    /// mode at creation regardless of what the parent holds.
    pub read_only: bool,
    /// Whether this role may spawn its own children (subject to depth).
    pub can_spawn: bool,
    /// Model override for this role; `None` inherits the parent's model.
    pub model: Option<String>,
    /// Provider override; `None` inherits the parent's provider binding.
    pub provider_id: Option<String>,
    /// Tool-loop bound for the role's turns; `None` keeps the executor
    /// default.
    pub max_iterations: Option<u32>,
    /// Free-form capability summary surfaced in `subagent_list` output.
    pub capability: Option<String>,
}

impl Default for RoleSpec {
    fn default() -> Self {
        Self {
            id: String::new(),
            description: String::new(),
            prompt: String::new(),
            allowed_tools: vec![],
            read_only: true,
            can_spawn: false,
            model: None,
            provider_id: None,
            max_iterations: None,
            capability: None,
        }
    }
}

/// Global fan-out bounds, enforced from durable rows at spawn time.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SubagentRoleLimits {
    /// Child depth allowed BELOW the root session (root children are
    /// depth 1). 0 disables spawning entirely.
    pub max_depth: i64,
    /// Global cap on non-terminal subagent rows across every parent.
    pub max_concurrency: i64,
    /// Cap on non-terminal children per parent session.
    pub max_per_parent: i64,
    /// Poll cadence for `subagent_await`, in milliseconds.
    pub await_poll_ms: u64,
    /// Hard bound on one `subagent_await` call, in milliseconds; a caller
    /// may ask for less, never more.
    pub max_await_ms: u64,
    /// Wall-clock bound on an exclusive-workspace lease before the reaper
    /// force-expires it (a wedged child cannot hold the workspace forever).
    pub lease_ttl_ms: i64,
    /// When true, a parent turn settling with non-terminal children cancels
    /// them (coopatively, through the session service).
    pub cancel_orphans: bool,
    /// Optional cap on provider requests reserved by ALL subagents below one
    /// root session; `None` means unlimited. Provider requests only: token
    /// and monetary budgets are not enforced.
    pub max_tree_provider_requests: Option<u64>,
}

impl Default for SubagentRoleLimits {
    fn default() -> Self {
        Self {
            max_depth: 2,
            max_concurrency: 8,
            max_per_parent: 4,
            await_poll_ms: 150,
            max_await_ms: 600_000,
            lease_ttl_ms: 900_000,
            cancel_orphans: true,
            max_tree_provider_requests: None,
        }
    }
}

/// The whole subagent subsystem configuration, hot-reloadable through the
/// service's `configure` entry point. `enabled` is the master switch: while
/// false the contributor offers NO tools, so sessions cannot delegate.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SubagentConfig {
    pub enabled: bool,
    #[serde(default)]
    pub limits: SubagentRoleLimits,
    /// Role table; built-ins are preloaded, host configuration may override
    /// any field or add custom roles.
    #[serde(default)]
    pub roles: BTreeMap<String, RoleSpec>,
    /// Guidance appended to every session's system prompt while enabled —
    /// the mechanism by which BOTH Chat and Coding sessions learn to
    /// delegate autonomously.
    pub delegation_guidance: Option<String>,
}

impl SubagentConfig {
    /// Effective configuration: an enabled/limited-but-role-less document
    /// falls back to the built-in role table, so a host can flip the feature
    /// on with `{}` and still get the five shipped roles.
    pub fn effective(mut self) -> Self {
        if self.roles.is_empty() {
            self.roles = default_role_specs();
        } else {
            // A host-authored partial table may name a built-in role without
            // restating it fully; unknown names are left alone (custom roles
            // are legitimate), but every built-in must still exist — a
            // deleted built-in is a configuration error the caller must see,
            // so it is re-materialized from defaults.
            for role in BUILTIN_ROLES {
                self.roles
                    .entry(role.to_owned())
                    .or_insert_with(|| default_role_specs().remove(role).expect("built-in"));
            }
        }
        if self.limits.max_depth < 0
            || self.limits.max_concurrency < 1
            || self.limits.max_per_parent < 1
            || self.limits.await_poll_ms < 10
            || self.limits.max_await_ms < self.limits.await_poll_ms
            || self.limits.lease_ttl_ms < 1_000
        {
            self.limits = SubagentRoleLimits::default();
        }
        if self
            .delegation_guidance
            .as_deref()
            .is_none_or(str::is_empty)
        {
            self.delegation_guidance = Some(default_delegation_guidance().to_owned());
        }
        self
    }

    pub fn role(&self, id: &str) -> Option<&RoleSpec> {
        self.roles.get(id)
    }
}

/// The five shipped roles. Read-only by default; only `implementer` may
/// write, and `general` is the pass-through for hosts that want the parent's
/// full ceiling (still bounded by the host guard chain and the mode ceiling).
pub fn default_role_specs() -> BTreeMap<String, RoleSpec> {
    let mut roles = BTreeMap::new();
    roles.insert(
        "general".into(),
        RoleSpec {
            id: "general".into(),
            description: "General-purpose delegate inheriting the parent's tool ceiling; use for \
                          self-contained multi-step work that benefits from a separate context."
                .into(),
            prompt: "You are a subagent working one delegated task to completion. Stay inside \
                     the assignment; report a complete, self-contained result as your final \
                     message."
                .into(),
            allowed_tools: vec!["*".into()],
            read_only: false,
            can_spawn: true,
            ..Default::default()
        },
    );
    roles.insert(
        "researcher".into(),
        RoleSpec {
            id: "researcher".into(),
            description: "Investigates questions against the repository and reports findings \
                          with evidence; read-only, may nest explorer children."
                .into(),
            prompt: "You are a research subagent. Answer the assignment from primary evidence \
                     (files, searches, symbols). Every claim you report must cite the file and \
                     location that supports it. Do not modify anything."
                .into(),
            allowed_tools: read_only_tool_set(),
            read_only: true,
            can_spawn: true,
            capability: Some("read-only investigation with evidence".into()),
            ..Default::default()
        },
    );
    roles.insert(
        "explorer".into(),
        RoleSpec {
            id: "explorer".into(),
            description: "Fast read-only codebase orientation: files, search, symbols, history. \
                          Returns maps and pointers, not judgements."
                .into(),
            prompt: "You are an exploration subagent. Locate and summarize; never modify files \
                     or run commands. Return precise paths and line references."
                .into(),
            allowed_tools: explorer_tool_set(),
            read_only: true,
            can_spawn: false,
            capability: Some("read-only orientation".into()),
            ..Default::default()
        },
    );
    roles.insert(
        "implementer".into(),
        RoleSpec {
            id: "implementer".into(),
            description: "Executes a scoped code change inside the parent's permission mode: \
                          edits, tests, reports the diff it made."
                .into(),
            prompt: "You are an implementation subagent. Deliver exactly the scoped change \
                     requested — no drive-by refactors. Run the relevant tests when available \
                     and report what you changed and what you verified."
                .into(),
            allowed_tools: {
                let mut set = explorer_tool_set();
                for name in [
                    "coding:edit_file",
                    "coding:write_file",
                    "coding:run_command",
                    "coding:run_tests",
                    "coding:submit_plan",
                ] {
                    set.push(name.into());
                }
                set
            },
            read_only: false,
            can_spawn: true,
            capability: Some("scoped workspace implementation".into()),
            ..Default::default()
        },
    );
    roles.insert(
        "reviewer".into(),
        RoleSpec {
            id: "reviewer".into(),
            description: "Read-only code review of a described change or diff: risks, defects, \
                          missing tests, regressions, with file/line evidence."
                .into(),
            prompt: "You are a review subagent. Judge only what the assignment points at. \
                     Findings must be concrete and evidenced (file, location, why it matters); \
                     say nothing you have not read. You cannot modify anything."
                .into(),
            allowed_tools: explorer_tool_set(),
            read_only: true,
            can_spawn: false,
            capability: Some("read-only review".into()),
            ..Default::default()
        },
    );
    roles
}

fn explorer_tool_set() -> Vec<String> {
    [
        "coding:read_file",
        "coding:list_dir",
        "coding:find_files",
        "coding:code_search",
        "coding:repo_map",
        "coding:git_inspect",
        "lsp_definition",
        "lsp_references",
        "lsp_hover",
        "lsp_symbols",
        "lsp_implementation",
        "lsp_diagnostics",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

fn read_only_tool_set() -> Vec<String> {
    explorer_tool_set()
}

pub fn default_delegation_guidance() -> &'static str {
    "Delegation: you can spawn subagents with subagent_spawn (roles: researcher, explorer, \
     implementer, reviewer, general) to run independent child sessions in parallel. Delegate \
     when a chunk of work is self-contained, read-heavy, or benefits from a fresh context \
     (investigation, exploration, review, an isolated implementation slice); keep work inline \
     when it is short, sequential, or needs your accumulated judgement. Spawn independent \
     children together, then subagent_await for their results. Children inherit your workspace \
     and permission ceiling; read-only roles cannot write. Report child findings with their \
     evidence."
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_cover_all_role_names() {
        let roles = default_role_specs();
        for id in BUILTIN_ROLES {
            assert!(roles.contains_key(id), "missing built-in role {id}");
        }
        assert!(
            roles["implementer"]
                .allowed_tools
                .contains(&"coding:edit_file".to_owned())
        );
        assert!(roles["reviewer"].read_only);
        assert!(!roles["reviewer"].can_spawn);
    }

    #[test]
    fn effective_fills_defaults_and_rejects_broken_limits() {
        let configured = SubagentConfig {
            enabled: true,
            limits: SubagentRoleLimits {
                max_depth: -3,
                ..Default::default()
            },
            ..Default::default()
        }
        .effective();
        assert!(configured.enabled);
        assert_eq!(
            configured.limits.max_depth,
            SubagentRoleLimits::default().max_depth
        );
        assert_eq!(configured.roles.len(), BUILTIN_ROLES.len());
        assert!(configured.delegation_guidance.is_some());
    }

    #[test]
    fn partial_role_table_rematerializes_builtins_and_keeps_customs() {
        let mut roles = BTreeMap::new();
        roles.insert(
            "auditor".to_owned(),
            RoleSpec {
                id: "auditor".into(),
                allowed_tools: vec!["coding:read_file".into()],
                ..Default::default()
            },
        );
        let config = SubagentConfig {
            enabled: true,
            roles,
            ..Default::default()
        }
        .effective();
        assert!(config.roles.contains_key("auditor"));
        for id in BUILTIN_ROLES {
            assert!(config.roles.contains_key(id));
        }
        assert!(config.role("auditor").is_some_and(|r| r.read_only));
    }
}
