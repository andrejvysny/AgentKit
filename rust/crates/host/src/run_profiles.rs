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

/// Per-unit framing overhead: role/delimiter tokens a unit costs on top
/// of its text, so many tiny units cannot slip under the budget.
const UNIT_FRAMING_TOKENS: u64 = 4;

/// Version tag of the compilation rules sealed into the profile.
const COMPILATION_POLICY: &str = "v2";

/// Conservative text estimate: `ceil(chars / 4)`, at least 1 when
/// non-empty, 0 when empty.
fn estimate_tokens(text: &str) -> u64 {
    if text.is_empty() {
        return 0;
    }
    let chars = u64::try_from(text.chars().count()).unwrap_or(u64::MAX);
    chars.div_ceil(4).max(1)
}

/// The ONE place a unit's cost is computed: text plus framing for every
/// non-empty unit.
fn unit_tokens(unit: &ContextUnit) -> u64 {
    match estimate_tokens(&unit.text) {
        0 => 0,
        text => text.saturating_add(UNIT_FRAMING_TOKENS),
    }
}

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

fn safe_window(window: Option<u64>) -> u64 {
    window
        .filter(|value| *value > 0 && *value <= MAX_SAFE_INTEGER)
        .unwrap_or(crate::FALLBACK_CONTEXT_WINDOW_TOKENS as u64)
}

/// The provider the host application owns and pins itself: the embedding
/// app's registry is the authority, so AgentKit's catalogue is not read.
/// The host re-checks its own registry at execution.
#[derive(Clone, Debug)]
pub struct HostProviderPin {
    pub provider_id: String,
    pub model_id: String,
    pub provider_kind: String,
    pub local: bool,
    pub connection_generation: Option<String>,
    pub tool_calling: bool,
    pub vision: bool,
    pub streaming: bool,
    pub context_window_tokens: Option<u64>,
}

/// Provider, model and capability after resolution, whichever authority
/// produced them; everything downstream is shared.
struct ResolvedProvider {
    provider_id: String,
    model_id: String,
    tool_calling: bool,
    vision: bool,
    streaming: bool,
    window: u64,
    supports_tool_calling_model: Option<bool>,
    authority: &'static str,
}

/// Capability validated BEFORE admission against recorded/pinned
/// evidence; returns continuation compatibility.
fn check_capability(
    resolved: &ResolvedProvider,
    task: &TaskProfileOverrides,
    tools: &[String],
) -> HostResult<bool> {
    if !tools.is_empty()
        && (!resolved.tool_calling || resolved.supports_tool_calling_model == Some(false))
    {
        return Err(refused("unsupported_tools:provider_lacks_tool_calling"));
    }
    if task.requires_vision && !resolved.vision {
        return Err(refused("unsupported_vision"));
    }
    if task.requires_continuation && !resolved.streaming {
        return Err(refused("provider_continuation_incompatible"));
    }
    Ok(resolved.streaming && (tools.is_empty() || resolved.tool_calling))
}

/// Bounded compilation. Never seals an over-budget profile: the kept
/// total is re-derived from the keep flags and asserted against budget.
fn compile_context(
    compiled: &mut [CompiledSource],
    budget: u64,
    truncate: bool,
) -> HostResult<Option<Value>> {
    let all = |compiled: &[CompiledSource]| -> Vec<(bool, u64)> {
        compiled
            .iter()
            .flat_map(|source| source.units.iter())
            .map(|unit| (unit.required, unit_tokens(unit)))
            .collect()
    };
    let units = all(compiled);
    let needed: u64 = units.iter().map(|(_, tokens)| *tokens).sum();
    let mut truncation = None;
    if needed > budget {
        if !truncate {
            // Overflow is a REFUSAL unless the caller put an explicit
            // truncation policy on record. Nothing is dropped silently.
            return Err(refused(format!(
                "context_overflow_truncation_required:needed {needed} tokens, {budget} fit"
            )));
        }
        // Pass 1: required units must fit — truncation is not an answer
        // to impossible material.
        let required: u64 = units
            .iter()
            .filter(|(required, _)| *required)
            .map(|(_, tokens)| *tokens)
            .sum();
        if required > budget {
            return Err(refused(format!(
                "context_overflow_unavoidable:{required} required tokens exceed the {budget}-token budget"
            )));
        }
        // Pass 2: optional units in original order, whole units only (a
        // tool-call pair is ONE unit); `used` already holds all required.
        let (mut used, mut dropped) = (required, 0u64);
        for source in compiled.iter_mut() {
            source.kept = source
                .units
                .iter()
                .map(|unit| {
                    let tokens = unit_tokens(unit);
                    let keep = unit.required || used.saturating_add(tokens) <= budget;
                    if keep && !unit.required {
                        used = used.saturating_add(tokens);
                    }
                    if !keep {
                        dropped += 1;
                    }
                    keep
                })
                .collect();
        }
        truncation = Some(json!({
            "mode": "explicit",
            "droppedUnits": dropped,
            "toolPairsPreserved": true,
        }));
    } else {
        for source in compiled.iter_mut() {
            source.kept = vec![true; source.units.len()];
        }
    }
    let kept_total: u64 = compiled
        .iter()
        .flat_map(|source| source.units.iter().zip(&source.kept))
        .filter(|(_, kept)| **kept)
        .map(|(unit, _)| unit_tokens(unit))
        .sum();
    if kept_total > budget {
        return Err(refused(format!(
            "context_compilation_invariant:kept {kept_total} tokens exceed the {budget}-token budget"
        )));
    }
    Ok(truncation)
}

fn unit_kind(unit: &ContextUnit) -> &'static str {
    match unit.kind {
        ContextUnitKind::Text => "text",
        ContextUnitKind::ToolPair => "toolPair",
    }
}

fn kept_units(source: &CompiledSource) -> impl Iterator<Item = &ContextUnit> {
    source
        .units
        .iter()
        .zip(&source.kept)
        .filter(|(_, kept)| **kept)
        .map(|(unit, _)| unit)
}

/// sha256 over the KEPT units, so the seal binds the admitted content
/// itself, not just ids and lengths.
fn content_digest(source: &CompiledSource) -> String {
    let units: Vec<Value> = kept_units(source).map(compiled_unit).collect();
    compiled_content_digest(&units).expect("compiled units are well-formed")
}

fn compiled_unit(unit: &ContextUnit) -> Value {
    json!({
        "kind": unit_kind(unit),
        "text": unit.text,
        "citations": unit.citations,
        "required": unit.required,
    })
}

/// The `contentDigest` of one `compiledContext` entry's `units`
/// (`{kind, text, citations, required}` objects), for executing hosts to
/// verify the content they deliver. Each unit is encoded as the positional
/// array `[kind, text, citations, required]` so the digest never depends on
/// JSON object key order (serde_json's `preserve_order` feature). `None`
/// when a unit is malformed.
pub fn compiled_content_digest(units: &[Value]) -> Option<String> {
    let mut encoded = Vec::with_capacity(units.len());
    for unit in units {
        let citations = unit["citations"].as_array()?;
        if !citations.iter().all(Value::is_string) {
            return None;
        }
        encoded.push(json!([
            unit["kind"].as_str()?,
            unit["text"].as_str()?,
            citations,
            unit["required"].as_bool()?,
        ]));
    }
    serde_json::to_string(&Value::Array(encoded))
        .ok()
        .map(|text| value_digest(&text))
}

/// Seal one compiled source: kept tokens, preserved citations, explicit
/// truncation markers. Dropped citations vanish with their unit; kept
/// citations are NEVER silently dropped.
fn seal_source(source: &CompiledSource) -> Value {
    let mut tokens = 0u64;
    let mut citations: Vec<String> = Vec::new();
    for unit in kept_units(source) {
        tokens = tokens.saturating_add(unit_tokens(unit));
        for citation in &unit.citations {
            if !citations.iter().any(|kept| kept == citation) {
                citations.push(citation.clone());
            }
        }
    }
    json!({
        "id": source.id,
        "kind": source.kind,
        "revision": source.revision,
        "tokens": tokens,
        "truncated": source.kept.iter().any(|kept| !*kept),
        "citations": citations,
        "contentDigest": content_digest(source),
    })
}

/// The exact admitted content, for the executing host to deliver.
fn compiled_entry(source: &CompiledSource) -> Value {
    let units: Vec<Value> = kept_units(source).map(compiled_unit).collect();
    json!({
        "sourceId": source.id,
        "kind": source.kind,
        "revision": source.revision,
        "units": units,
    })
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
        self.resolve_inner(agent_id, now, project, task, approvals, None)
            .await
    }

    /// Resolve with a HOST-AUTHORITY provider: the embedding application
    /// owns its provider registry, so provider/model/capability come from
    /// the pin with NO catalogue reads. Task provider/model overrides,
    /// when present, must equal the pin's. Everything after provider
    /// resolution is the same code as [`Self::resolve`].
    pub async fn resolve_with_host_provider(
        &self,
        agent_id: &str,
        now: i64,
        project: &ProjectSelection,
        task: &TaskProfileOverrides,
        approvals: &[ApprovalGrantRef],
        host: &HostProviderPin,
    ) -> HostResult<PinnedRunProfile> {
        self.resolve_inner(agent_id, now, project, task, approvals, Some(host))
            .await
    }

    async fn resolve_inner(
        &self,
        agent_id: &str,
        now: i64,
        project: &ProjectSelection,
        task: &TaskProfileOverrides,
        approvals: &[ApprovalGrantRef],
        host: Option<&HostProviderPin>,
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
        let resolved = match host {
            Some(pin) => self.host_provider(pin, task, &mut provenance)?,
            None => self.catalog_provider(&agent, task, &mut provenance).await?,
        };
        let (effective_mode, effective_tools) = self.permissions(&agent, task, &mut provenance)?;
        let continuation_compatible = check_capability(&resolved, task, &effective_tools)?;
        let skills = self.pinned_skills(&agent).await?;
        let mut compiled = self.collect_sources(&agent, project).await?;

        let reserve = 16_384u64.min(resolved.window / 2);
        let fixed = estimate_tokens(&agent.definition.instructions);
        let budget = resolved
            .window
            .saturating_sub(reserve)
            .saturating_sub(fixed);
        let truncation = compile_context(&mut compiled, budget, task.truncation.is_some())?;
        let context_sources: Vec<Value> = compiled.iter().map(seal_source).collect();
        let compiled_context: Vec<Value> = compiled.iter().map(compiled_entry).collect();
        let budgets = self.budgets(&agent, task, &mut provenance)?;

        // -- environment: names and digests, never values ---------------
        let vars: Vec<Value> = self
            .defaults
            .environment
            .iter()
            .map(|(name, value)| json!({"name": name, "valueDigest": value_digest(value)}))
            .collect();

        let mut provider = json!({
            "providerId": resolved.provider_id,
            "modelId": resolved.model_id,
            "localOnly": self.defaults.local_only,
            "fallbacks": self.defaults.fallback_providers,
            "authority": resolved.authority,
        });
        if let (Some(pin), Some(object)) = (host, provider.as_object_mut()) {
            object.insert("kind".into(), json!(pin.provider_kind));
            object.insert(
                "connectionGeneration".into(),
                json!(pin.connection_generation),
            );
        }
        let profile = json!({
            "version": 1,
            "agentId": agent.agent_id,
            "agentRevision": agent.current_revision,
            "provider": provider,
            "capability": {
                "toolCalling": resolved.tool_calling,
                "vision": resolved.vision,
                "streaming": resolved.streaming,
                "continuationCompatible": continuation_compatible,
                "contextWindowTokens": resolved.window,
            },
            "skills": skills,
            "contextSources": context_sources,
            "compiledContext": compiled_context,
            "compilationPolicy": COMPILATION_POLICY,
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

    /// Host-authority provider: values come from the pin, never from the
    /// catalogue. Overrides may only restate the pin.
    fn host_provider(
        &self,
        host: &HostProviderPin,
        task: &TaskProfileOverrides,
        provenance: &mut serde_json::Map<String, Value>,
    ) -> HostResult<ResolvedProvider> {
        if host.provider_id.trim().is_empty() {
            return Err(refused("provider_unnamed"));
        }
        if host.model_id.trim().is_empty() {
            return Err(refused("model_unresolved"));
        }
        if task
            .provider_id
            .as_deref()
            .is_some_and(|requested| requested != host.provider_id)
            || task
                .model
                .as_deref()
                .is_some_and(|model| !model.is_empty() && model != host.model_id)
        {
            return Err(refused("host_provider_mismatch"));
        }
        if self.defaults.local_only && !host.local && !self.is_fallback(&host.provider_id) {
            return Err(refused(format!(
                "silent_local_to_cloud_refused:{}",
                host.provider_id
            )));
        }
        provenance.insert("provider".into(), json!("host"));
        provenance.insert("model".into(), json!("host"));
        Ok(ResolvedProvider {
            provider_id: host.provider_id.clone(),
            model_id: host.model_id.clone(),
            tool_calling: host.tool_calling,
            vision: host.vision,
            streaming: host.streaming,
            window: safe_window(host.context_window_tokens),
            supports_tool_calling_model: None,
            authority: "host",
        })
    }

    fn is_fallback(&self, provider_id: &str) -> bool {
        self.defaults
            .fallback_providers
            .iter()
            .any(|configured| configured == provider_id)
    }

    /// Catalogue provider: the original routing, model precedence and
    /// recorded-capability reads.
    async fn catalog_provider(
        &self,
        agent: &agentkit_store::AgentRecord,
        task: &TaskProfileOverrides,
        provenance: &mut serde_json::Map<String, Value>,
    ) -> HostResult<ResolvedProvider> {
        // -- provider & routing: only explicitly configured fallbacks ---
        let provider_id = match task.provider_id.as_deref() {
            Some(requested) if requested != self.defaults.provider_id => {
                if !self.is_fallback(requested) {
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
        if self.defaults.local_only && !is_local && !self.is_fallback(&provider_id) {
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
        let window = model_row
            .as_ref()
            .and_then(|row| row["contextWindowTokens"].as_u64())
            .or_else(|| capabilities["maxContextTokens"].as_u64());
        Ok(ResolvedProvider {
            provider_id,
            model_id,
            tool_calling: capabilities["toolCalling"].as_bool() == Some(true),
            vision: capabilities["vision"].as_bool() == Some(true),
            streaming: capabilities["streaming"].as_bool() == Some(true),
            window: safe_window(window),
            supports_tool_calling_model: model_row
                .as_ref()
                .and_then(|row| row["supportsToolCalling"].as_bool()),
            authority: "catalog",
        })
    }

    /// Permission ceiling (agent ceiling ∩ default ∩ request) and the
    /// tool set, which must live inside the agent's ceiling.
    fn permissions(
        &self,
        agent: &agentkit_store::AgentRecord,
        task: &TaskProfileOverrides,
        provenance: &mut serde_json::Map<String, Value>,
    ) -> HostResult<(agentkit_core::permission::CodingMode, Vec<String>)> {
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
        Ok((effective_mode, effective_tools))
    }

    /// Skills: pinned versions resolve or the run refuses.
    async fn pinned_skills(&self, agent: &agentkit_store::AgentRecord) -> HostResult<Vec<Value>> {
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
        Ok(skills)
    }

    /// Context sources: selection selects, grants authorize.
    async fn collect_sources(
        &self,
        agent: &agentkit_store::AgentRecord,
        project: &ProjectSelection,
    ) -> HostResult<Vec<CompiledSource>> {
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
                Some(
                    self.skill_source_revision(agent, source, &mut units)
                        .await?,
                )
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
        Ok(compiled)
    }

    async fn skill_source_revision(
        &self,
        agent: &agentkit_store::AgentRecord,
        source: &SelectedSource,
        units: &mut Vec<ContextUnit>,
    ) -> HostResult<String> {
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
        Ok(revision)
    }

    /// Budgets: ceilings bound, precedence fills. With neither ceiling
    /// nor default, a requested value stands (provenance "task"). Every
    /// requested and stored value must be positive.
    fn budgets(
        &self,
        agent: &agentkit_store::AgentRecord,
        task: &TaskProfileOverrides,
        provenance: &mut serde_json::Map<String, Value>,
    ) -> HostResult<serde_json::Map<String, Value>> {
        let mut budgets = serde_json::Map::new();
        let (a, d, t) = (
            &agent.definition.budgets,
            &self.defaults.budgets,
            &task.budgets,
        );
        for (key, ceiling, default, requested) in [
            (
                "maxToolIterations",
                a.max_tool_iterations,
                d.max_tool_iterations,
                t.max_tool_iterations,
            ),
            (
                "maxToolCallsPerIteration",
                a.max_tool_calls_per_iteration,
                d.max_tool_calls_per_iteration,
                t.max_tool_calls_per_iteration,
            ),
            (
                "maxOutputTokens",
                a.max_output_tokens,
                d.max_output_tokens,
                t.max_output_tokens,
            ),
            (
                "attemptBudgetMs",
                a.attempt_budget_ms,
                d.attempt_budget_ms,
                t.attempt_budget_ms,
            ),
        ] {
            if [ceiling, default, requested]
                .into_iter()
                .flatten()
                .any(|value| value <= 0)
            {
                return Err(invalid_overrides(format!("budget {key} must be positive")));
            }
            let limit = ceiling.or(default);
            if let (Some(limit), Some(value)) = (limit, requested)
                && value > limit
            {
                return Err(refused(format!("budget_ceiling_exceeded:{key}")));
            }
            if requested.is_some() {
                provenance.insert(key.to_string(), json!("task"));
            }
            if let Some(value) = requested.or(limit) {
                budgets.insert(key.to_string(), json!(value));
            }
        }
        Ok(budgets)
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
                            "contentDigest": source["contentDigest"],
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
                "authority": profile["provider"]["authority"],
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
