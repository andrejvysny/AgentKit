//! The session tool manifest (roadmap 1.3, CE-04/CE-05, ADR 0028).
//!
//! At a session's first turn every tool the contributors offer is sorted by
//! name, hashed and stored in the [`AiSessionPrefix`]. Later turns send the
//! stored definitions, in that order, whatever the contributors answer now:
//! a contributor that is slow or gone leaves its tools in the array (calls
//! fail with `tool_unavailable`), a late contributor joins the deferred
//! catalog, and a guard refuses at call time instead of removing a tool.
//! Room for later fields (dialect, toolset) is the prefix's own optional
//! fields; the hash covers the definitions as stored.

use crate::ToolDefinition;
use crate::deferred::{Classification, TOOL_CALL, is_meta_tool, toolset_index};
use crate::toolset::ToolsetView;
use agentkit_contracts::{AiSessionPrefix, AiToolDefinition, JsonDeserialize, Optional};
use agentkit_kernel::canonical_sha256;
use serde_json::{Value, json};
use std::collections::BTreeSet;

/// Bumped when the hashed shape of a manifest changes.
const MANIFEST_FORMAT: u32 = 1;

/// Prefix `extra` key: the deferred tools that existed when the session
/// froze. A tool that joined the catalog later is not among them, so
/// delegation never counts it as part of the parent's authority.
const DEFERRED_AT_FREEZE_KEY: &str = "deferredAtFreeze";

/// The definitions a session was frozen with, sorted by name.
#[derive(Clone, Debug)]
pub struct ToolManifest {
    definitions: Vec<ToolDefinition>,
    hash: String,
    /// Stands in for a recorded manifest that failed validation: it holds
    /// no tool, and a stamp never overwrites the record it stands for.
    invalid: bool,
}

impl ToolManifest {
    /// Sort `candidates` by name (a repeated name keeps its first
    /// definition) and hash them. A definition that is not a valid wire tool
    /// definition is left out: no provider could be sent it.
    pub fn build(candidates: &[ToolDefinition]) -> Self {
        Self::from_sorted(sorted_unique(candidates))
    }

    fn from_sorted(definitions: Vec<ToolDefinition>) -> Self {
        // Hash the contract form: that is what a prefix stores and what
        // `from_prefix` reads back, so both sides hash the same value.
        let definitions = normalized(definitions);
        let hash = manifest_hash(&definitions);
        Self {
            definitions,
            hash,
            invalid: false,
        }
    }

    fn invalid() -> Self {
        Self {
            invalid: true,
            ..Self::from_sorted(vec![])
        }
    }

    /// The manifest recorded in `prefix`, if it has one that still matches
    /// its own hash. A record that does not match is no record.
    pub fn from_prefix(prefix: &AiSessionPrefix) -> Option<Self> {
        let Optional::Present(entries) = &prefix.tool_manifest else {
            return None;
        };
        let definitions = from_contract(entries)?;
        let manifest = Self::from_sorted(sorted_unique(&definitions));
        match &prefix.tool_manifest_hash {
            Optional::Present(hash) if *hash == manifest.hash => Some(manifest),
            _ => None,
        }
    }

    /// The definitions to send, in manifest (name) order.
    pub fn definitions(&self) -> &[ToolDefinition] {
        &self.definitions
    }

    pub fn hash(&self) -> &str {
        &self.hash
    }

    pub fn contains(&self, name: &str) -> bool {
        self.definitions
            .binary_search_by(|definition| definition.name.as_str().cmp(name))
            .is_ok()
    }
}

/// SHA-256 over the canonical JSON of the sorted definitions, tagged with the
/// manifest format.
pub fn manifest_hash(sorted: &[ToolDefinition]) -> String {
    let tools = serde_json::to_value(sorted).expect("tool definitions serialize");
    canonical_sha256(&json!({"format": MANIFEST_FORMAT, "tools": tools}))
}

/// What a turn's tools mean for the session's frozen surface.
#[derive(Clone, Debug)]
pub struct ToolSurface {
    /// The frozen manifest (new on a session's first turn).
    pub manifest: ToolManifest,
    /// The deferred catalog, sorted by name.
    pub deferred: Vec<ToolDefinition>,
    /// Names that joined the deferred catalog this turn.
    pub added: Vec<String>,
    /// Manifest tools no contributor offers this turn.
    pub unavailable: Vec<String>,
    /// Direct tools moved to the deferred catalog on the first turn because
    /// the profile's visible-tool bound left no room (T2.1).
    pub demoted: Vec<String>,
    /// True when the manifest was built or rebuilt this turn.
    pub frozen_now: bool,
}

impl ToolSurface {
    /// Reconcile this turn's `candidates` (every tool the contributors offer
    /// now, withheld ones included) with the prefix recorded earlier. Every
    /// candidate is sent: no tool is deferred.
    pub fn reconcile(prior: Option<&AiSessionPrefix>, candidates: &[ToolDefinition]) -> Self {
        Self::reconcile_with(prior, candidates, None)
    }

    /// Like [`ToolSurface::reconcile`], and on a session's first turn the
    /// `classification` (roadmap 2.3) splits the candidates: direct tools and,
    /// when something is deferred, the meta tools form the manifest; the rest
    /// form the deferred catalog. Later turns follow the recorded prefix.
    pub fn reconcile_with(
        prior: Option<&AiSessionPrefix>,
        candidates: &[ToolDefinition],
        classification: Option<&Classification>,
    ) -> Self {
        let Some(manifest) = prior.and_then(ToolManifest::from_prefix) else {
            // A record that exists but does not validate is never replaced
            // by a fresh freeze: the session gets no tools instead.
            if prior.is_some_and(records_manifest) {
                return Self::invalid(candidates);
            }
            return Self::first(candidates, classification);
        };
        let mut deferred = prior.and_then(recorded_deferred).unwrap_or_default();
        let known: BTreeSet<&str> = deferred.iter().map(|d| d.name.as_str()).collect();
        let offered: BTreeSet<&str> = candidates.iter().map(|d| d.name.as_str()).collect();
        // The meta tools are never "late": the session either froze with
        // them or never will.
        let late: Vec<ToolDefinition> = sorted_unique(candidates)
            .into_iter()
            .filter(|d| !is_meta_tool(&d.name))
            .filter(|d| !manifest.contains(&d.name) && !known.contains(d.name.as_str()))
            .collect();
        let added = late.iter().map(|d| d.name.clone()).collect();
        deferred.extend(late);
        deferred.sort_by(|a, b| a.name.cmp(&b.name));
        let unavailable = manifest
            .definitions()
            .iter()
            .filter(|d| !offered.contains(d.name.as_str()))
            .map(|d| d.name.clone())
            .collect();
        Self {
            manifest,
            deferred,
            added,
            unavailable,
            demoted: vec![],
            frozen_now: false,
        }
    }

    /// The surface of a session whose recorded manifest is invalid: no tool
    /// is callable and every offered tool is reported unavailable.
    fn invalid(candidates: &[ToolDefinition]) -> Self {
        let unavailable = sorted_unique(candidates)
            .into_iter()
            .filter(|d| !is_meta_tool(&d.name))
            .map(|d| d.name)
            .collect();
        Self {
            manifest: ToolManifest::invalid(),
            deferred: vec![],
            added: vec![],
            unavailable,
            demoted: vec![],
            frozen_now: false,
        }
    }

    fn first(candidates: &[ToolDefinition], classification: Option<&Classification>) -> Self {
        let (manifest, deferred, demoted) = match classification {
            None => (ToolManifest::build(candidates), vec![], vec![]),
            Some(classification) => {
                let split = classification.split(candidates);
                let sent: Vec<ToolDefinition> =
                    split.direct.into_iter().chain(split.meta).collect();
                (
                    ToolManifest::build(&sent),
                    sorted_unique(&split.deferred),
                    split.demoted,
                )
            }
        };
        Self {
            manifest,
            deferred,
            added: vec![],
            unavailable: vec![],
            demoted,
            frozen_now: true,
        }
    }

    /// The S2 catalog index (one line per deferred namespace), when the
    /// session's array carries the meta tools to use it.
    pub fn index(&self) -> Option<String> {
        self.index_with(&ToolsetView::default())
    }

    /// [`ToolSurface::index`] with the session's toolsets: one card line per
    /// toolset that has a deferred tool or a guide (T4.1).
    pub fn index_with(&self, view: &ToolsetView) -> Option<String> {
        if !self.manifest.contains(TOOL_CALL) {
            return None;
        }
        toolset_index(self.manifest.definitions(), &self.deferred, view)
    }

    /// Names of the deferred tools, for the registry's not-yet-callable set.
    pub fn deferred_names(&self) -> Vec<String> {
        self.deferred.iter().map(|d| d.name.clone()).collect()
    }

    /// Write the manifest, its hash and the catalog into `prefix`.
    pub fn stamp(&self, prefix: &mut AiSessionPrefix) {
        if self.manifest.invalid {
            return;
        }
        if self.frozen_now {
            let names: Vec<Value> = self.deferred.iter().map(|d| json!(d.name)).collect();
            prefix
                .extra
                .insert(DEFERRED_AT_FREEZE_KEY.to_owned(), Value::Array(names));
        }
        prefix.tool_manifest_hash = Optional::Present(self.manifest.hash.clone());
        prefix.tool_manifest = Optional::Present(to_contract(self.manifest.definitions()));
        prefix.deferred_tools = if self.deferred.is_empty() {
            Optional::Absent
        } else {
            Optional::Present(to_contract(&self.deferred))
        };
    }
}

/// True when `prefix` carries any manifest record, valid or not.
fn records_manifest(prefix: &AiSessionPrefix) -> bool {
    matches!(prefix.tool_manifest, Optional::Present(_))
        || matches!(prefix.tool_manifest_hash, Optional::Present(_))
}

/// Every tool a session's prefix records, direct and deferred, meta tools
/// left out, sorted by name. `None` when the prefix holds no valid manifest
/// (missing, malformed or hash mismatch: delegation fails closed on all
/// three). Deferred tools count only when they were in the catalog at the
/// freeze; a late-added tool never widens a child's ceiling. Delegation
/// intersects a child's authority with this set at spawn.
pub fn recorded_tools(prefix: &AiSessionPrefix) -> Option<Vec<ToolDefinition>> {
    let manifest = ToolManifest::from_prefix(prefix)?;
    let mut tools: Vec<ToolDefinition> = manifest
        .definitions()
        .iter()
        .filter(|definition| !is_meta_tool(&definition.name))
        .cloned()
        .collect();
    let deferred = match &prefix.deferred_tools {
        Optional::Present(entries) => from_contract(entries)?,
        Optional::Absent => vec![],
    };
    let at_freeze: BTreeSet<&str> = prefix
        .extra
        .get(DEFERRED_AT_FREEZE_KEY)
        .and_then(Value::as_array)
        .map(|names| names.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    tools.extend(
        deferred
            .into_iter()
            .filter(|definition| at_freeze.contains(definition.name.as_str())),
    );
    Some(sorted_unique(&tools))
}

fn recorded_deferred(prefix: &AiSessionPrefix) -> Option<Vec<ToolDefinition>> {
    match &prefix.deferred_tools {
        Optional::Present(entries) => from_contract(entries),
        Optional::Absent => None,
    }
}

fn sorted_unique(candidates: &[ToolDefinition]) -> Vec<ToolDefinition> {
    let mut seen = BTreeSet::new();
    let mut definitions: Vec<ToolDefinition> = candidates
        .iter()
        .filter(|definition| as_contract(definition).is_some())
        .filter(|definition| seen.insert(definition.name.clone()))
        .cloned()
        .collect();
    definitions.sort_by(|a, b| a.name.cmp(&b.name));
    definitions
}

fn as_contract(definition: &ToolDefinition) -> Option<AiToolDefinition> {
    let value: Value = serde_json::to_value(definition).ok()?;
    AiToolDefinition::from_json(value).ok()
}

fn to_contract(definitions: &[ToolDefinition]) -> Vec<AiToolDefinition> {
    definitions.iter().filter_map(as_contract).collect()
}

/// The definitions as the contract stores them. A definition the contract
/// cannot hold is dropped, as `build` already does.
fn normalized(definitions: Vec<ToolDefinition>) -> Vec<ToolDefinition> {
    from_contract(&to_contract(&definitions)).unwrap_or(definitions)
}

fn from_contract(entries: &[AiToolDefinition]) -> Option<Vec<ToolDefinition>> {
    entries
        .iter()
        .map(|entry| serde_json::from_value(serde_json::to_value(entry).ok()?).ok())
        .collect()
}
