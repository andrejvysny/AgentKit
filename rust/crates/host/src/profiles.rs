//! Host-owned profiles. Sessions freeze resolved data, never mutable registry references.
use crate::{HostError, HostResult};
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
}

fn invalid(message: impl std::fmt::Display) -> HostError {
    HostError::Execution(format!("invalid_profile: {message}"))
}

impl AgentProfile {
    pub fn validate(&self) -> HostResult<()> {
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

    pub fn filter_tools(&self, registry: &ToolRegistry) -> HostResult<Arc<ToolRegistry>> {
        let mut filtered = ToolRegistry::new();
        for definition in registry.definitions() {
            if !self.tools.contains(&definition.name) {
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
    pub fn new(overrides: &[Value]) -> HostResult<Self> {
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
            .collect::<HostResult<_>>()?;
        Ok(Self { profiles })
    }

    pub fn list(&self) -> Vec<AgentProfile> {
        self.profiles.values().cloned().collect()
    }

    pub fn resolve(&self, id: &str) -> HostResult<AgentProfile> {
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
            if matches!(key.as_str(), "limits" | "resources") && value.is_object() {
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
) -> HostResult<Value> {
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
pub(crate) fn task_profile(payload: &Value) -> HostResult<Option<AgentProfile>> {
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
}
