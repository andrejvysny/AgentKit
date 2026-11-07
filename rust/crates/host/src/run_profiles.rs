//! AGENTKIT-89: the effective run profile — ONE immutable document that
//! resolves host defaults, the agent definition (AGENTKIT-87 revision
//! ledger), the project's context selection, and the task's overrides.
//!
//! This is the native profile service extended, not a parallel one: the
//! resolver reads every input through the service that owns it —
//! [`agentkit_store::SqliteStore::get_agent`] for the definition
//! revision, the AGENTKIT-65/68 provider catalogue and capability rows
//! for model/provider capability, the AGENTKIT-80s managed-skill rows
//! for pinned skill versions, the standing-grant machinery for approvals
//! — and seals the result with the store's single digest authority
//! ([`agentkit_store::run_profile_digest`]). The seal rides the SAME
//! admission transaction as the AGENTKIT-88 binding
//! ([`crate::TeammateService::admit`]); revocation of anything referenced
//! fences NEW dispatch through the store's one check-at-use evaluator.
//!
//! Refusals, all stable-coded under `run_profile_refused:`:
//! - `fallback_not_configured:<p>` / `silent_local_to_cloud_refused:<p>`
//!   — only explicitly configured fallbacks route; a local-only policy
//!   never silently lands a cloud provider;
//! - `unsupported_tools:<detail>` / `unsupported_vision` /
//!   `provider_continuation_incompatible` / `provider_capability_missing`
//!   — capability is validated BEFORE admission, against recorded
//!   evidence, never by probing the provider;
//! - `context_overflow_truncation_required` / `context_overflow_unavoidable`
//!   — bounded source compilation: overflow without an explicit truncation
//!   policy is a refusal, and compilation never splits a tool-call pair or
//!   drops a citation silently;
//! - `permission_expansion_refused:<detail>` / `budget_ceiling_exceeded:<k>`
//!   — ceilings bound; overrides below them resolve by precedence
//!   (defaults < agent definition < task);
//! - `source_not_granted:<id>` — project selection selects, the agent's
//!   own grants authorize: selecting an unrelated memory or file refuses.
//!
//! Secrets have no path through this layer: provider rows are read for
//! whitelisted metadata only (no `apiKey`, no headers), environment
//! values are recorded as digests, and secret references stay references.

use crate::{HostError, HostResult};
use agentkit_store::{AgentBudgets, PinnedRunProfile, SqliteStore, run_profile_digest};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, HashSet};

/// The host's default layer of the resolution: what governs where the
/// agent definition and the task say nothing.
#[derive(Clone, Debug)]
pub struct RunProfileDefaults {
    /// The default provider identity (catalogue id).
    pub provider_id: String,
    /// Default model when neither the agent nor the task names one.
    pub model: Option<String>,
    /// Default permission mode; the resolved mode never sits ABOVE the
    /// lower of this and the agent's ceiling.
    pub permission_mode: String,
    /// The default tool set, used when the agent lists no allow-list.
    pub tools: Vec<String>,
    /// Default budget limits; absent agent ceilings fall back to these.
    pub budgets: AgentBudgets,
    /// Host environment the profile may record (names + value digests
    /// only — the values themselves never enter the profile).
    pub environment: BTreeMap<String, String>,
    /// Routing policy: with `local_only`, dispatch stays on local
    /// providers unless a provider is an explicitly configured fallback.
    pub local_only: bool,
    /// The explicitly configured fallback providers: the ONLY providers
    /// a task may route to besides the default, and the only way a
    /// local-only policy can ever reach a cloud provider.
    pub fallback_providers: Vec<String>,
}

impl Default for RunProfileDefaults {
    fn default() -> Self {
        Self {
            provider_id: "native".into(),
            model: None,
            permission_mode: "ask".into(),
            tools: vec![],
            budgets: AgentBudgets::default(),
            environment: BTreeMap::new(),
            local_only: false,
            fallback_providers: vec![],
        }
    }
}

/// The project's context selection: sources the project WANTS in scope.
/// Selection never grants access — the agent's own definition grants
/// (skills pinned in its selection, resources listed in its `resources`).
#[derive(Clone, Debug, Default)]
pub struct ProjectSelection {
    pub sources: Vec<SelectedSource>,
}

/// One selected context source. `units` are the compilation units the
/// caller wants composed (texts and ATOMIC tool-call pairs); an empty
/// list for a skill source compiles its pinned document as one text unit.
#[derive(Clone, Debug)]
pub struct SelectedSource {
    pub id: String,
    pub kind: SelectedSourceKind,
    pub units: Vec<ContextUnit>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectedSourceKind {
    /// A managed skill, pinned at a revision in the agent's selection.
    Skill,
    /// A resource granted in the agent's own `resources` list.
    Resource,
}

/// One unit of compiled context. `ToolPair` units (a tool call and its
/// result as one document) are ATOMIC: explicit truncation drops whole
/// units and never splits a pair. `required` units are refused-overflow
/// material: they must fit or the resolution refuses.
#[derive(Clone, Debug)]
pub struct ContextUnit {
    pub kind: ContextUnitKind,
    pub text: String,
    pub citations: Vec<String>,
    pub required: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContextUnitKind {
    Text,
    ToolPair,
}

/// The task's override layer. `deny_unknown_fields` is the smuggling
/// gate: there is no field for a secret value, a credential, or a
/// permission grant, and an envelope that tries answers as invalid
/// overrides.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskProfileOverrides {
    #[serde(default)]
    pub provider_id: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    /// The requested tool set; every name must already be inside the
    /// agent's ceiling — a name outside it is permission expansion.
    #[serde(default)]
    pub tools: Option<Vec<String>>,
    #[serde(default)]
    pub requires_vision: bool,
    #[serde(default)]
    pub requires_continuation: bool,
    /// Requested mode; never above the agent's ceiling.
    #[serde(default)]
    pub permission_mode: Option<String>,
    /// Per-key budget requests; never above the agent's ceilings.
    #[serde(default)]
    pub budgets: AgentBudgets,
    /// The ONLY way a context overflow resolves: an explicit truncation
    /// policy. Without it, overflow is a refusal, not a silent drop.
    #[serde(default)]
    pub truncation: Option<TruncationPolicy>,
}

/// The explicit truncation policy: the caller's on-record decision that
/// bounded compilation may drop whole units.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TruncationPolicy {
    /// Only `"explicit"` is (and always is) the mode — there is no
    /// implicit or silent truncation to select.
    pub mode: String,
}

/// Standing approvals the run's admission was granted on, recorded so
/// revocation can fence dispatch at use.
#[derive(Clone, Debug)]
pub struct ApprovalGrantRef {
    pub session_id: String,
    pub grant_id: String,
    pub digest: String,
}

fn refused(reason: impl std::fmt::Display) -> HostError {
    HostError::Execution(format!("run_profile_refused:{reason}"))
}

fn invalid_overrides(detail: impl std::fmt::Display) -> HostError {
    HostError::Execution(format!("invalid_overrides: {detail}"))
}

/// The permission ladder. Higher ranks may attempt strictly more than
/// lower ones; an override may not climb above the lower of the agent
/// ceiling and the host default.
fn mode_rank(mode: agentkit_core::permission::CodingMode) -> u8 {
    use agentkit_core::permission::CodingMode::*;
    match mode {
        ReadOnly | Plan => 0,
        Ask => 1,
        AutoEdit => 2,
        FullAuto => 3,
    }
}

fn estimate_tokens(text: &str) -> u64 {
    u64::try_from(text.chars().count()).unwrap_or(u64::MAX) / 4
}

fn value_digest(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The local-provider vocabulary: kinds that never leave the machine.
/// Anything else in the catalogue is a cloud route.
const LOCAL_PROVIDER_KINDS: [&str; 4] = ["local", "ollama", "lmstudio", "llama.cpp"];

/// One source after grant validation, with its units and the fit verdict
/// the compilation pass wrote back.
struct CompiledSource {
    id: String,
    kind: &'static str,
    revision: Option<String>,
    units: Vec<ContextUnit>,
    kept: Vec<bool>,
}

#[derive(Clone)]
pub struct RunProfileService {
    store: SqliteStore,
    defaults: RunProfileDefaults,
}

impl RunProfileService {
    /// Build the resolver over the host's existing store and the host's
    /// default layer. No new registry: every durable read goes to the
    /// service that owns the data.
    pub fn new(store: SqliteStore, defaults: RunProfileDefaults) -> Self {
        Self { store, defaults }
    }

    pub fn store(&self) -> &SqliteStore {
        &self.store
    }

    /// Resolve the ONE effective run profile for one admission and seal
    /// it. Validation (tools, vision, context capacity, continuation
    /// compatibility, fallback legitimacy) all happens HERE, BEFORE
    /// admission; the returned pin is what the admission transaction
    /// persists, and what later dispatch re-checks at use.
    pub async fn resolve(
        &self,
        agent_id: &str,
        now: i64,
        project: &ProjectSelection,
        task: &TaskProfileOverrides,
        approvals: &[ApprovalGrantRef],
    ) -> HostResult<PinnedRunProfile> {
        if task
            .truncation
            .as_ref()
            .is_some_and(|policy| policy.mode != "explicit")
        {
            return Err(refused("truncation mode must be explicit"));
        }
        let agent = self
            .store
            .get_agent(agent_id.to_string())
            .await?
            .filter(|agent| agent.lifecycle != agentkit_store::AgentLifecycle::Deleted)
            .ok_or_else(|| HostError::NotFound(format!("agent {agent_id}")))?;
        let mut provenance = serde_json::Map::new();

        // -- provider & routing: only explicitly configured fallbacks ---
        let provider_id = match task.provider_id.as_deref() {
            Some(requested) if requested != self.defaults.provider_id => {
                if !self
                    .defaults
                    .fallback_providers
                    .iter()
                    .any(|configured| configured == requested)
                {
                    // A different provider nobody configured is a silent
                    // fallback — refused, whatever the direction.
                    return Err(refused(format!("fallback_not_configured:{requested}")));
                }
                provenance.insert("provider".into(), json!("task"));
                requested.to_owned()
            }
            _ => {
                provenance.insert("provider".into(), json!("default"));
                self.defaults.provider_id.clone()
            }
        };
        if provider_id.trim().is_empty() {
            return Err(refused("provider_unnamed"));
        }
        let provider = self
            .store
            .get_provider(&provider_id)
            .await?
            .ok_or_else(|| refused(format!("unknown_provider:{provider_id}")))?;
        if provider["enabled"].as_bool() != Some(true) {
            return Err(refused(format!("provider_disabled:{provider_id}")));
        }
        // Local-only policy is absolute except where a cloud provider is
        // an EXPLICITLY configured fallback: the same list that authorizes
        // the route at all. Nothing else can move a local run to the cloud.
        let is_local = provider["kind"]
            .as_str()
            .is_some_and(|kind| LOCAL_PROVIDER_KINDS.contains(&kind));
        if self.defaults.local_only
            && !is_local
            && !self
                .defaults
                .fallback_providers
                .iter()
                .any(|configured| configured == &provider_id)
        {
            return Err(refused(format!(
                "silent_local_to_cloud_refused:{provider_id}"
            )));
        }

        // -- model: defaults < agent definition < task ------------------
        let (model_id, model_provenance) = match (
            task.model.as_deref(),
            agent.definition.model.as_deref(),
            self.defaults.model.as_deref(),
        ) {
            (Some(model), _, _) if !model.is_empty() => (model.to_owned(), "task"),
            (_, Some(model), _) if !model.is_empty() => (model.to_owned(), "agent"),
            (_, _, Some(model)) if !model.is_empty() => (model.to_owned(), "default"),
            _ => return Err(refused("model_unresolved")),
        };
        provenance.insert("model".into(), json!(model_provenance));
        let model_row = self
            .store
            .list_provider_models(&provider_id)
            .await?
            .into_iter()
            .find(|row| row["modelId"].as_str() == Some(model_id.as_str()));

        // -- capability: recorded evidence, validated BEFORE admission ---
        let capabilities = self
            .store
            .get_provider_capabilities(&provider_id)
            .await?
            .ok_or_else(|| refused(format!("provider_capability_missing:{provider_id}")))?;
        let tool_calling = capabilities["toolCalling"].as_bool() == Some(true);
        let vision = capabilities["vision"].as_bool() == Some(true);
        let streaming = capabilities["streaming"].as_bool() == Some(true);

        // -- permission ceiling: agent ceiling ∩ default ∩ request ------
        let default_mode =
            agentkit_core::permission::CodingMode::parse(&self.defaults.permission_mode)
                .ok_or_else(|| refused("default_permission_mode"))?;
        let agent_mode =
            agentkit_core::permission::CodingMode::parse(&agent.definition.permission_mode)
                .ok_or_else(|| {
                    refused(format!(
                        "agent_permission_mode:{}",
                        agent.definition.permission_mode
                    ))
                })?;
        let requested_mode = task
            .permission_mode
            .as_deref()
            .map(|mode| {
                agentkit_core::permission::CodingMode::parse(mode)
                    .ok_or_else(|| invalid_overrides(format!("unknown permissionMode {mode}")))
            })
            .transpose()?;
        let ceiling_rank = mode_rank(default_mode).min(mode_rank(agent_mode));
        if requested_mode.is_some_and(|mode| mode_rank(mode) > ceiling_rank) {
            // Permission expansion: a task may narrow, never climb.
            return Err(refused(format!(
                "permission_expansion_refused:permissionMode {}",
                task.permission_mode.as_deref().unwrap_or_default()
            )));
        }
        let effective_mode = [Some(default_mode), Some(agent_mode), requested_mode]
            .into_iter()
            .flatten()
            .min_by_key(|mode| mode_rank(*mode))
            .unwrap_or(agent_mode);
        provenance.insert(
            "permissionMode".into(),
            json!(if requested_mode.is_some() {
                "task"
            } else if mode_rank(agent_mode) <= mode_rank(default_mode) {
                "agent"
            } else {
                "default"
            }),
        );

        // -- tools: requested set must live inside the agent's ceiling --
        let ceiling_tools: &Vec<String> = if agent.definition.allowed_tools.is_empty() {
            &self.defaults.tools
        } else {
            &agent.definition.allowed_tools
        };
        let effective_tools: Vec<String> = match task.tools.as_ref() {
            Some(requested) => {
                let mut seen = HashSet::new();
                for tool in requested {
                    if tool.is_empty() || !seen.insert(tool.as_str()) {
                        return Err(invalid_overrides("tools must be non-empty and unique"));
                    }
                    if !ceiling_tools.iter().any(|granted| granted == tool) {
                        return Err(refused(format!("permission_expansion_refused:tool {tool}")));
                    }
                }
                provenance.insert("tools".into(), json!("task"));
                requested.clone()
            }
            None => {
                provenance.insert(
                    "tools".into(),
                    json!(if agent.definition.allowed_tools.is_empty() {
                        "default"
                    } else {
                        "agent"
                    }),
                );
                ceiling_tools.clone()
            }
        };
        if !effective_tools.is_empty()
            && (!tool_calling
                || model_row
                    .as_ref()
                    .and_then(|row| row["supportsToolCalling"].as_bool())
                    == Some(false))
        {
            return Err(refused("unsupported_tools:provider_lacks_tool_calling"));
        }
        if task.requires_vision && !vision {
            return Err(refused("unsupported_vision"));
        }
        if task.requires_continuation && !streaming {
            return Err(refused("provider_continuation_incompatible"));
        }
        let continuation_compatible = streaming && (effective_tools.is_empty() || tool_calling);

        // -- skills: pinned versions resolve or the run refuses ---------
        let mut skills = Vec::new();
        for selection in &agent.definition.skills {
            let entry = self
                .store
                .get_skill(selection.id.clone())
                .await?
                .ok_or_else(|| refused(format!("skill_revoked:{}", selection.id)))?;
            if entry["revision"].as_str() != Some(selection.revision.as_str()) {
                // The pinned version no longer exists: a re-published
                // skill is a different document, never a silent upgrade.
                return Err(refused(format!("skill_revoked:{}", selection.id)));
            }
            skills.push(json!({"id": selection.id, "revision": selection.revision}));
        }

        // -- context sources: selection selects, grants authorize ------
        let mut compiled: Vec<CompiledSource> = Vec::new();
        for source in &project.sources {
            let granted = match source.kind {
                SelectedSourceKind::Skill => agent
                    .definition
                    .skills
                    .iter()
                    .any(|skill| skill.id == source.id),
                SelectedSourceKind::Resource => agent
                    .definition
                    .resources
                    .iter()
                    .any(|resource| resource == &source.id),
            };
            if !granted {
                // Project selection never grants access to unrelated
                // memory or files: only the agent's own definition does.
                return Err(refused(format!("source_not_granted:{}", source.id)));
            }
            let mut units = source.units.clone();
            let revision = if source.kind == SelectedSourceKind::Skill {
                let entry = self
                    .store
                    .get_skill(source.id.clone())
                    .await?
                    .ok_or_else(|| refused(format!("skill_revoked:{}", source.id)))?;
                let revision = entry["revision"]
                    .as_str()
                    .ok_or_else(|| refused(format!("skill_revoked:{}", source.id)))?
                    .to_owned();
                if agent
                    .definition
                    .skills
                    .iter()
                    .any(|skill| skill.id == source.id && skill.revision != revision)
                {
                    // Selected at a pinned revision, compiled at another:
                    // the pin, not the live row, is the contract.
                    return Err(refused(format!("skill_revoked:{}", source.id)));
                }
                if units.is_empty() {
                    let text = entry["content"]
                        .as_str()
                        .or_else(|| entry["instructions"].as_str())
                        .unwrap_or_default()
                        .to_owned();
                    units.push(ContextUnit {
                        kind: ContextUnitKind::Text,
                        text,
                        citations: vec![],
                        required: false,
                    });
                }
                Some(revision)
            } else {
                None
            };
            compiled.push(CompiledSource {
                id: source.id.clone(),
                kind: match source.kind {
                    SelectedSourceKind::Skill => "skill",
                    SelectedSourceKind::Resource => "resource",
                },
                revision,
                units,
                kept: vec![],
            });
        }

        // -- bounded compilation ----------------------------------------
        let window = model_row
            .as_ref()
            .and_then(|row| row["contextWindowTokens"].as_u64())
            .or_else(|| capabilities["maxContextTokens"].as_u64())
            .filter(|value| *value > 0 && *value <= 9_007_199_254_740_991)
            .unwrap_or(crate::FALLBACK_CONTEXT_WINDOW_TOKENS as u64);
        let reserve = 16_384u64.min(window / 2);
        let fixed = estimate_tokens(&agent.definition.instructions);
        let budget = window.saturating_sub(reserve).saturating_sub(fixed);
        let needed: u64 = compiled
            .iter()
            .flat_map(|source| source.units.iter())
            .map(|unit| estimate_tokens(&unit.text))
            .sum();
        let truncation: Option<Value> = if needed > budget {
            if task.truncation.is_none() {
                // Overflow is a REFUSAL unless the caller put an explicit
                // truncation policy on record. Nothing is dropped silently.
                return Err(refused(format!(
                    "context_overflow_truncation_required:needed {needed} tokens, {budget} fit"
                )));
            }
            // Explicit truncation: whole units only. Required units must
            // fit — truncation is not an answer to impossible material.
            let required_tokens: u64 = compiled
                .iter()
                .flat_map(|source| source.units.iter())
                .filter(|unit| unit.required)
                .map(|unit| estimate_tokens(&unit.text))
                .sum();
            if required_tokens > budget {
                return Err(refused(format!(
                    "context_overflow_unavoidable:{required_tokens} required tokens exceed the {budget}-token budget"
                )));
            }
            let mut used = 0u64;
            let mut dropped = 0u64;
            for source in &mut compiled {
                source.kept = Vec::with_capacity(source.units.len());
                for unit in &source.units {
                    let tokens = estimate_tokens(&unit.text);
                    // A tool-call pair is ONE unit — kept whole or
                    // dropped whole, never split across the cut.
                    let keep = unit.required || used.saturating_add(tokens) <= budget;
                    if keep {
                        used = used.saturating_add(tokens);
                    } else {
                        dropped += 1;
                    }
                    source.kept.push(keep);
                }
            }
            Some(json!({
                "mode": "explicit",
                "droppedUnits": dropped,
                // Units are atomic; a tool-call pair is one unit, so a
                // pair is never split by the cut.
                "toolPairsPreserved": true,
            }))
        } else {
            for source in &mut compiled {
                source.kept = vec![true; source.units.len()];
            }
            None
        };

        // Seal the compiled sources: kept tokens, preserved citations,
        // explicit truncation markers. Dropped citations vanish with
        // their unit; kept citations are NEVER silently dropped.
        let context_sources: Vec<Value> = compiled
            .iter()
            .map(|source| {
                let mut tokens = 0u64;
                let mut citations: Vec<String> = Vec::new();
                let mut truncated = false;
                for (unit, kept) in source.units.iter().zip(&source.kept) {
                    if *kept {
                        tokens = tokens.saturating_add(estimate_tokens(&unit.text));
                        for citation in &unit.citations {
                            if !citations.iter().any(|kept| kept == citation) {
                                citations.push(citation.clone());
                            }
                        }
                    } else {
                        truncated = true;
                    }
                }
                json!({
                    "id": source.id,
                    "kind": source.kind,
                    "revision": source.revision,
                    "tokens": tokens,
                    "truncated": truncated,
                    "citations": citations,
                })
            })
            .collect();

        // -- budgets: ceilings bound, precedence fills ------------------
        let mut budgets = serde_json::Map::new();
        for (key, ceiling, default, requested) in [
            (
                "maxToolIterations",
                agent.definition.budgets.max_tool_iterations,
                self.defaults.budgets.max_tool_iterations,
                task.budgets.max_tool_iterations,
            ),
            (
                "maxToolCallsPerIteration",
                agent.definition.budgets.max_tool_calls_per_iteration,
                self.defaults.budgets.max_tool_calls_per_iteration,
                task.budgets.max_tool_calls_per_iteration,
            ),
            (
                "maxOutputTokens",
                agent.definition.budgets.max_output_tokens,
                self.defaults.budgets.max_output_tokens,
                task.budgets.max_output_tokens,
            ),
            (
                "attemptBudgetMs",
                agent.definition.budgets.attempt_budget_ms,
                self.defaults.budgets.attempt_budget_ms,
                task.budgets.attempt_budget_ms,
            ),
        ] {
            let Some(ceiling) = ceiling.or(default) else {
                continue;
            };
            if requested.is_some_and(|value| value > ceiling) {
                return Err(refused(format!("budget_ceiling_exceeded:{key}")));
            }
            if requested.is_some() {
                provenance.insert(key.to_string(), json!("task"));
            }
            budgets.insert(key.to_string(), json!(requested.unwrap_or(ceiling)));
        }

        // -- environment: names and digests, never values ---------------
        let vars: Vec<Value> = self
            .defaults
            .environment
            .iter()
            .map(|(name, value)| json!({"name": name, "valueDigest": value_digest(value)}))
            .collect();

        let profile = json!({
            "version": 1,
            "agentId": agent.agent_id,
            "agentRevision": agent.current_revision,
            "provider": {
                "providerId": provider_id,
                "modelId": model_id,
                "localOnly": self.defaults.local_only,
                "fallbacks": self.defaults.fallback_providers,
            },
            "capability": {
                "toolCalling": tool_calling,
                "vision": vision,
                "streaming": streaming,
                "continuationCompatible": continuation_compatible,
                "contextWindowTokens": window,
            },
            "skills": skills,
            "contextSources": context_sources,
            "permissionCeiling": {
                "mode": effective_mode.as_str(),
                "tools": effective_tools,
            },
            "budgets": Value::Object(budgets),
            "environment": {"vars": vars},
            "approvals": approvals
                .iter()
                .map(|grant| {
                    json!({
                        "sessionId": grant.session_id,
                        "grantId": grant.grant_id,
                        "digest": grant.digest,
                    })
                })
                .collect::<Vec<Value>>(),
            "truncation": truncation,
            "provenance": Value::Object(provenance),
        });
        Ok(PinnedRunProfile {
            digest: run_profile_digest(&profile),
            profile,
            pinned_at: now,
        })
    }

    /// The SAFE effective-profile explanation for OneMind diagnostics:
    /// the pinned meaning projected through a whitelist, so no secret
    /// material (values, credentials, headers) and no private reasoning
    /// can ride along. The seal is recomputed before anything is said —
    /// an explanation never vouches for a tampered row.
    pub async fn explain(&self, task_id: &str) -> HostResult<Option<Value>> {
        let Some(task) = self.store.get_task(task_id).await? else {
            return Err(HostError::NotFound(format!("task {task_id}")));
        };
        let Some(pin) = task.run_profile.as_ref() else {
            return Ok(None);
        };
        if run_profile_digest(&pin.profile) != pin.digest {
            return Err(refused("digest_mismatch"));
        }
        let profile = &pin.profile;
        let sources: Vec<Value> = profile["contextSources"]
            .as_array()
            .map(|sources| {
                sources
                    .iter()
                    .map(|source| {
                        json!({
                            "id": source["id"],
                            "kind": source["kind"],
                            "tokens": source["tokens"],
                            "truncated": source["truncated"].as_bool().unwrap_or(false),
                            "citations": source["citations"],
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let refusal = self
            .store
            .run_profile_dispatch_refusal(task_id.to_owned())
            .await?;
        Ok(Some(json!({
            "taskId": task.task_id,
            "profileDigest": pin.digest,
            "pinnedAt": pin.pinned_at,
            "agentId": profile["agentId"],
            "agentRevision": profile["agentRevision"],
            "provider": {
                "providerId": profile["provider"]["providerId"],
                "modelId": profile["provider"]["modelId"],
                "localOnly": profile["provider"]["localOnly"],
                "fallbacks": profile["provider"]["fallbacks"],
            },
            "capability": profile["capability"],
            "skills": profile["skills"],
            "contextSources": sources,
            "permissionCeiling": profile["permissionCeiling"],
            "budgets": profile["budgets"],
            // Names only: value digests are opaque and stay out of the
            // human-facing explanation.
            "environment": {"vars": profile["environment"]["vars"].as_array()
                .map(|vars| {
                    vars.iter()
                        .map(|entry| json!({"name": entry["name"]}))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()},
            "truncation": profile["truncation"],
            "provenance": profile["provenance"],
            "dispatch": {
                "allowed": refusal.is_none(),
                "refusal": refusal,
            },
        })))
    }
}
