//! Host-owned profiles. Sessions freeze resolved data, never mutable registry references.
use crate::{SessionsError, SessionsResult};
use agentkit_core::{ReasoningEffort, ToolRegistry};
use agentkit_kernel::sha256_hex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashSet},
    sync::Arc,
};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentLimits {
    pub max_tool_iterations: Option<u32>,
    pub max_tool_calls_per_iteration: Option<usize>,
    pub history_limit: Option<usize>,
    pub attempt_budget_ms: Option<u64>,
    pub max_output_tokens: Option<u64>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentResources {
    #[serde(default)]
    pub skills: Vec<SkillSelection>,
    pub project_instructions: Option<bool>,
    #[serde(default)]
    pub resources: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SkillSelection {
    pub id: String,
    pub revision: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentProfile {
    pub id: String,
    pub kind: String,
    pub label: Option<String>,
    pub instructions: String,
    pub model: Option<String>,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub tools: Vec<String>,
    pub permission_mode: String,
    pub role: Option<String>,
    #[serde(default)]
    pub limits: AgentLimits,
    #[serde(default)]
    pub resources: AgentResources,
    /// Context defaults for the session; a session descriptor's own
    /// `context` values win. Skipped when absent so existing digests hold.
    #[serde(
        default,
        rename = "contextPolicy",
        skip_serializing_if = "Option::is_none"
    )]
    pub context_policy: Option<ContextPolicy>,
}

/// A profile's context defaults (spec 10): applied where the session
/// descriptor's `context` leaves the value unset.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContextPolicy {
    pub masking: Option<bool>,
    pub auto_compaction: Option<bool>,
}

/// One `AgentProfile.tools` entry: an exact tool name, `toolset:<id>`
/// (every tool of that toolset) or a name prefix ending in `*`.
enum ToolPattern<'a> {
    /// `*`: every tool the host staged. The host's contributors and guards
    /// already decide what a surface offers (OneMind Chat, spec 10 S2).
    All,
    Exact(&'a str),
    Toolset(&'a str),
    Prefix(&'a str),
}

impl<'a> ToolPattern<'a> {
    /// `None` for an entry outside the grammar.
    fn parse(entry: &'a str) -> Option<Self> {
        if entry == "*" {
            return Some(Self::All);
        }
        if let Some(id) = entry.strip_prefix("toolset:") {
            return (!id.is_empty() && !id.contains('*')).then_some(Self::Toolset(id));
        }
        if let Some(prefix) = entry.strip_suffix('*') {
            return (!prefix.is_empty() && !prefix.contains('*')).then_some(Self::Prefix(prefix));
        }
        (!entry.is_empty() && !entry.contains('*')).then_some(Self::Exact(entry))
    }

    fn matches(&self, name: &str, toolset: Option<&str>) -> bool {
        match self {
            Self::All => true,
            Self::Exact(exact) => name == *exact,
            Self::Toolset(id) => toolset == Some(*id),
            Self::Prefix(prefix) => name.starts_with(prefix),
        }
    }
}

fn invalid(message: impl std::fmt::Display) -> SessionsError {
    SessionsError::InvalidProfile(message.to_string())
}

impl AgentProfile {
    pub fn validate(&self) -> SessionsResult<()> {
        if self.id.is_empty()
            || agentkit_core::permission::CodingMode::parse(&self.permission_mode).is_none()
            || !["chat", "coding", "research", "automation"].contains(&self.kind.as_str())
        {
            return Err(invalid("invalid identity, kind, or permission mode"));
        }
        if self.tools.iter().any(String::is_empty)
            || self.tools.iter().collect::<HashSet<_>>().len() != self.tools.len()
        {
            return Err(invalid("tool names must be non-empty and unique"));
        }
        if self
            .tools
            .iter()
            .any(|entry| ToolPattern::parse(entry).is_none())
        {
            return Err(invalid(
                "tool entries are a name, toolset:<id>, or a prefix ending in *",
            ));
        }
        if self
            .role
            .as_deref()
            .is_some_and(|r| !["implementer", "reviewer", "supervisor"].contains(&r))
        {
            return Err(invalid("unknown role"));
        }
        if self.role.as_deref() == Some("reviewer")
            && self
                .tools
                .iter()
                .any(|n| matches!(n.as_str(), "edit_file" | "write_file"))
        {
            return Err(invalid("reviewer may not list edit/write tools"));
        }
        if self.role.as_deref() == Some("supervisor")
            && self.tools.iter().any(|n| {
                n != "submit_plan"
                    && ["write", "edit", "run_", "apply", "delete", "remove"]
                        .iter()
                        .any(|hint| n.contains(hint))
            })
        {
            return Err(invalid("supervisor may not list non-read tools"));
        }
        for value in serde_json::to_value(&self.limits)
            .map_err(invalid)?
            .as_object()
            .expect("limits")
            .values()
            .filter(|v| !v.is_null())
        {
            if value
                .as_u64()
                .is_none_or(|v| v == 0 || v > 9_007_199_254_740_991)
            {
                return Err(invalid("limits must be positive safe integers"));
            }
        }
        if self
            .limits
            .attempt_budget_ms
            .is_some_and(|v| v > 2_147_483_647)
        {
            return Err(invalid("attemptBudgetMs must be at most 2147483647"));
        }
        if self
            .resources
            .skills
            .iter()
            .any(|s| s.id.is_empty() || s.revision.is_empty())
        {
            return Err(invalid("skills require pinned identity and revision"));
        }
        Ok(())
    }

    pub fn digest(&self) -> String {
        sha256_hex(serde_json::to_vec(self).expect("profile serialization"))
    }

    /// True when an entry of `tools` selects tool `name` of toolset `toolset`.
    fn selects(&self, name: &str, toolset: Option<&str>) -> bool {
        self.tools
            .iter()
            .filter_map(|entry| ToolPattern::parse(entry))
            .any(|pattern| pattern.matches(name, toolset))
    }

    pub fn filter_tools(&self, registry: &ToolRegistry) -> SessionsResult<Arc<ToolRegistry>> {
        let mut filtered = ToolRegistry::new();
        for definition in registry.definitions() {
            let toolset = registry.toolset_of(&definition.name);
            if !self.selects(&definition.name, toolset.as_deref()) {
                continue;
            }
            let class = agentkit_core::permission::classify_capability(&definition.capability);
            let writes = definition.effect != "read";
            if self.role.as_deref() == Some("supervisor")
                && (writes
                    || class
                        .is_some_and(|value| value != agentkit_core::permission::ToolClass::Read))
                && definition.name != "submit_plan"
            {
                continue;
            }
            if self.role.as_deref() == Some("reviewer")
                && (class == Some(agentkit_core::permission::ToolClass::Write)
                    || (writes && class != Some(agentkit_core::permission::ToolClass::Exec)))
            {
                continue;
            }
            if let Some(tool) = registry.get(&definition.name) {
                filtered.register(tool).map_err(invalid)?;
            }
        }
        // Withheld tools stay offered; a call is refused with the reason.
        for (definition, reason) in registry.withheld_definitions() {
            let toolset = registry.toolset_of_definition(&definition);
            if self.selects(&definition.name, Some(toolset.as_str())) {
                filtered.withhold_tool(definition, &reason);
            }
        }
        // Toolsets (guides and cards), retired names and aliases belong to the
        // session, not to a tool: keep what still applies to the tools left.
        filtered.inherit_metadata(registry, &|name| {
            self.selects(name, registry.toolset_of(name).as_deref())
        });
        Ok(Arc::new(filtered))
    }
}

#[derive(Clone)]
pub struct ProfileRegistry {
    profiles: BTreeMap<String, AgentProfile>,
}

impl Default for ProfileRegistry {
    fn default() -> Self {
        Self::new(&[]).expect("built-in profiles")
    }
}

impl ProfileRegistry {
    /// Overrides inherit built-in fields of the same id. `extends` selects another parent.
    pub fn new(overrides: &[Value]) -> SessionsResult<Self> {
        let builtins: Vec<Value> =
            serde_json::from_str(include_str!("profiles_builtin.json")).expect("built-in data");
        let base: BTreeMap<String, Value> = builtins
            .into_iter()
            .map(|v| (v["id"].as_str().expect("built-in id").to_owned(), v))
            .collect();
        let mut definitions = BTreeMap::new();
        for value in overrides {
            let id = value["id"]
                .as_str()
                .filter(|id| !id.is_empty())
                .ok_or_else(|| invalid("id required"))?;
            if definitions.insert(id.to_owned(), value.clone()).is_some() {
                return Err(invalid("duplicate profile override"));
            }
        }
        let mut resolved = base.clone();
        for id in definitions.keys() {
            resolve(id, &base, &definitions, &mut resolved, &mut HashSet::new())?;
        }
        let profiles = resolved
            .into_iter()
            .map(|(id, value)| {
                let profile: AgentProfile = serde_json::from_value(value).map_err(invalid)?;
                profile.validate()?;
                Ok((id, profile))
            })
            .collect::<SessionsResult<_>>()?;
        Ok(Self { profiles })
    }

    pub fn list(&self) -> Vec<AgentProfile> {
        self.profiles.values().cloned().collect()
    }

    pub fn resolve(&self, id: &str) -> SessionsResult<AgentProfile> {
        self.profiles
            .get(id)
            .cloned()
            .ok_or_else(|| invalid(format!("unknown profile {id}")))
    }
}

fn merge(base: &mut Value, extra: &Value) {
    if let (Some(base), Some(extra)) = (base.as_object_mut(), extra.as_object()) {
        for (key, value) in extra {
            if key == "extends" {
                continue;
            }
            if matches!(key.as_str(), "limits" | "resources" | "contextPolicy") && value.is_object()
            {
                merge(
                    base.entry(key.clone())
                        .or_insert_with(|| serde_json::json!({})),
                    value,
                );
            } else {
                base.insert(key.clone(), value.clone());
            }
        }
    }
}

fn resolve(
    id: &str,
    base: &BTreeMap<String, Value>,
    definitions: &BTreeMap<String, Value>,
    resolved: &mut BTreeMap<String, Value>,
    visiting: &mut HashSet<String>,
) -> SessionsResult<Value> {
    if !visiting.insert(id.to_owned()) {
        return Err(invalid("inheritance cycle"));
    }
    let Some(extra) = definitions.get(id) else {
        visiting.remove(id);
        return base
            .get(id)
            .cloned()
            .ok_or_else(|| invalid(format!("unknown parent {id}")));
    };
    let mut value = if let Some(parent) = extra.get("extends") {
        let parent = parent
            .as_str()
            .ok_or_else(|| invalid("extends must be a string"))?;
        resolve(parent, base, definitions, resolved, visiting)?
    } else {
        base.get(id)
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}))
    };
    merge(&mut value, extra);
    visiting.remove(id);
    resolved.insert(id.to_owned(), value.clone());
    Ok(value)
}

/// A task's immutable profile must match the descriptor identity captured at admission.
pub fn task_profile(payload: &Value) -> SessionsResult<Option<AgentProfile>> {
    let Some(snapshot) = payload.get("profileSnapshot") else {
        return Ok(None);
    };
    let profile: AgentProfile = serde_json::from_value(snapshot.clone()).map_err(invalid)?;
    profile.validate()?;
    if payload["profileDigest"].as_str() != Some(profile.digest().as_str()) {
        return Err(invalid("snapshot digest mismatch"));
    }
    if let Some(descriptor) = payload.get("sessionDescriptor") {
        let descriptor =
            crate::read_session_descriptor(&serde_json::json!({"agentkit.session":descriptor}))
                .ok_or_else(|| invalid("invalid session profile binding"))?;
        if descriptor["profile"]["id"].as_str() != Some(profile.id.as_str())
            || descriptor["profile"]["digest"].as_str() != Some(profile.digest().as_str())
        {
            return Err(invalid("session profile binding mismatch"));
        }
    }
    Ok(Some(profile))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn overrides_inherit_defaults_and_freeze_identity() {
        let registry = ProfileRegistry::new(&[
            json!({"id":"custom","extends":"coding","limits":{"maxToolIterations":2}}),
        ])
        .unwrap();
        let profile = registry.resolve("custom").unwrap();
        assert_eq!(profile.limits.max_tool_iterations, Some(2));
        assert_eq!(profile.limits.history_limit, None);
        assert!(profile.tools.contains(&"edit_file".into()));
        let mut payload = json!({"profileSnapshot":profile,"profileDigest":profile.digest()});
        assert!(task_profile(&payload).unwrap().is_some());
        payload["profileSnapshot"]["instructions"] = "tamper".into();
        assert!(task_profile(&payload).is_err());
        assert!(registry.resolve("unknown").is_err());
    }
    #[test]
    fn cycles_and_invalid_limits_fail_closed() {
        assert!(
            ProfileRegistry::new(&[
                json!({"id":"a","extends":"b"}),
                json!({"id":"b","extends":"a"})
            ])
            .is_err()
        );
        assert!(ProfileRegistry::new(&[json!({"id":"chat","limits":{"historyLimit":0}})]).is_err());
    }

    struct Named {
        definition: agentkit_core::ToolDefinition,
        toolset: Option<&'static str>,
    }

    #[async_trait::async_trait]
    impl agentkit_core::Tool for Named {
        fn definition(&self) -> &agentkit_core::ToolDefinition {
            &self.definition
        }
        fn toolset(&self) -> Option<&str> {
            self.toolset
        }
        async fn execute(
            &self,
            context: agentkit_core::ToolContext,
            _: Value,
        ) -> Result<agentkit_core::ToolResult, agentkit_core::CoreError> {
            Ok(agentkit_core::ToolResult {
                ok: true,
                data: None,
                sources: vec![],
                warnings: vec![],
                truncated: false,
                limits: context.limits,
                model_data: None,
                summary: None,
                status: None,
                model_content: None,
            })
        }
    }

    fn named(name: &str, capability: &str, toolset: Option<&'static str>) -> Arc<Named> {
        let definition = serde_json::from_value(json!({
            "name": name, "version": "1", "effect": "read", "capability": capability,
            "description": name, "inputSchema": {"type": "object"}
        }))
        .unwrap();
        Arc::new(Named {
            definition,
            toolset,
        })
    }

    /// `read_file` declares the files toolset, `list_dir` is listed in it,
    /// the MCP tools fall into their name's namespace.
    fn staged() -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        for tool in [
            named("read_file", "fs.read", Some("files")),
            named("list_dir", "fs.read", None),
            named("mcp__github__search", "mcp.call", None),
            named("mcp__slack__post", "mcp.call", None),
            named("run_command", "process.exec", None),
        ] {
            registry.register(tool).unwrap();
        }
        let mut files = agentkit_core::Toolset::new("files", "Files", "Read workspace files.");
        files.tools = vec!["list_dir".into()];
        registry.register_toolset(files);
        registry
    }

    /// Based on `coding` (implementer): no role ceiling hides a matched tool.
    fn filtered_names(tools: Value) -> Vec<String> {
        let registry =
            ProfileRegistry::new(&[json!({"id":"custom","extends":"coding","tools":tools})])
                .unwrap();
        let mut names: Vec<String> = registry
            .resolve("custom")
            .unwrap()
            .filter_tools(&staged())
            .unwrap()
            .definitions()
            .into_iter()
            .map(|definition| definition.name)
            .collect();
        names.sort();
        names
    }

    #[test]
    fn filter_tools_resolves_toolsets_and_prefix_globs() {
        assert_eq!(
            filtered_names(json!(["toolset:files", "mcp__github__*"])),
            vec!["list_dir", "mcp__github__search", "read_file"]
        );
        assert_eq!(
            filtered_names(json!(["mcp__*"])),
            vec!["mcp__github__search", "mcp__slack__post"]
        );
        assert_eq!(
            filtered_names(json!(["toolset:mcp__slack"])),
            vec!["mcp__slack__post"]
        );
        // Exact names keep their meaning.
        assert_eq!(filtered_names(json!(["run_command"])), vec!["run_command"]);
        assert!(filtered_names(json!(["toolset:unknown", "nothing_*"])).is_empty());
        // `*` keeps every staged tool.
        let all = filtered_names(json!(["*"]));
        assert!(all.iter().any(|name| name == "mcp__slack__post"), "{all:?}");
        assert!(all.iter().any(|name| name == "run_command"), "{all:?}");
    }

    #[test]
    fn tool_entries_outside_the_grammar_are_rejected() {
        for tools in [
            json!(["toolset:"]),
            json!(["mcp*__x"]),
            json!(["mcp__**"]),
            json!(["toolset:files*"]),
        ] {
            assert!(
                ProfileRegistry::new(&[json!({"id":"custom","extends":"chat","tools":tools})])
                    .is_err(),
                "{tools}"
            );
        }
    }

    #[test]
    fn context_policy_is_optional_and_left_out_of_the_digest_when_absent() {
        let registry = ProfileRegistry::new(&[
            json!({"id":"quiet","extends":"chat","contextPolicy":{"masking":false}}),
            json!({"id":"quieter","extends":"quiet","contextPolicy":{"autoCompaction":true}}),
        ])
        .unwrap();
        let chat = registry.resolve("chat").unwrap();
        assert!(chat.context_policy.is_none());
        assert!(
            serde_json::to_value(&chat)
                .unwrap()
                .get("contextPolicy")
                .is_none()
        );
        let quieter = registry.resolve("quieter").unwrap().context_policy.unwrap();
        assert_eq!(quieter.masking, Some(false));
        assert_eq!(quieter.auto_compaction, Some(true));
        assert!(
            ProfileRegistry::new(&[json!({"id":"x","extends":"chat","contextPolicy":{"other":1}})])
                .is_err()
        );
    }
}
