//! Deferred tools (roadmap 2.3, CE-08/CE-09, ADR 0031): which tools a session
//! sends in its tools array, and the catalog of the rest.
//!
//! A session freezes a *direct* set (sent every turn) and a *deferred* set
//! (listed in the catalog, found with `tool_search`, described with
//! `tool_describe`, run with `tool_call`). The classification is an input,
//! never a profile type: hosts pass a [`DirectSet`], tools may carry a
//! [`ToolLoading`] hint, and a manifest small enough to fit the budget is all
//! direct with the meta tools omitted. Discovery is not authorization: a
//! catalog entry says only whether the tool is callable this turn.

use crate::toolset::ToolsetView;
use crate::{Dialect, ToolDefinition};
use agentkit_contracts::{AiModelLoadMode, AiModelToolProfile};
use serde_json::Value;
use std::collections::BTreeSet;
use std::sync::{Arc, OnceLock};

pub const TOOL_SEARCH: &str = "tool_search";
pub const TOOL_DESCRIBE: &str = "tool_describe";
pub const TOOL_CALL: &str = "tool_call";
pub const TOOLSET_LOAD: &str = "toolset_load";
/// The meta tools, in the order they are documented.
pub const META_TOOLS: [&str; 4] = [TOOL_SEARCH, TOOL_DESCRIBE, TOOL_CALL, TOOLSET_LOAD];

pub fn is_meta_tool(name: &str) -> bool {
    META_TOOLS.contains(&name)
}

/// A tool's own wish: sent every turn, or listed in the catalog. A host's
/// [`DirectSet`] overrides it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ToolLoading {
    #[default]
    Direct,
    Deferred,
}

/// The per-profile override: exactly these tools are direct. `Unspecified`
/// leaves the decision to each tool's [`ToolLoading`] hint.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum DirectSet {
    #[default]
    Unspecified,
    Names(BTreeSet<String>),
}

impl DirectSet {
    pub fn names<I, S>(names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::Names(names.into_iter().map(Into::into).collect())
    }
}

/// What happens to a toolset's tools once `toolset_load` has loaded it
/// (T4.1, ADR 0033). Frozen with the session profile.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LoadMode {
    /// The tools array never changes; loaded tools run through `tool_call`.
    #[default]
    Indirect,
    /// Loaded tools join the tools array at the next turn boundary.
    Promote,
}

/// Everything the freeze needs to decide direct or deferred.
#[derive(Clone, Debug)]
pub struct Classification {
    pub direct: DirectSet,
    /// Tools whose own [`ToolLoading`] is `Deferred` (from the registry).
    pub deferred_hints: BTreeSet<String>,
    /// A manifest within both limits is all direct and needs no meta tools.
    pub max_tokens: usize,
    pub max_definitions: usize,
    /// Advertise the meta tools even when every tool is direct: a late
    /// contributor (a connected server) can add tools after the freeze and
    /// only the meta tools reach them.
    pub keep_meta: bool,
    pub load_mode: LoadMode,
    /// The most tools the rendered array may hold, meta tools included, after
    /// the dialect expanded its tools (`git_inspect` is four wire tools).
    /// `None` leaves the array to `max_definitions` alone.
    pub visible_bound: Option<usize>,
    /// The wire names of each direct tool, in the profile's priority order.
    /// A tool over the bound is demoted whole, last group first.
    pub direct_groups: Vec<Vec<String>>,
}

impl Default for Classification {
    fn default() -> Self {
        Self {
            direct: DirectSet::Unspecified,
            deferred_hints: BTreeSet::new(),
            max_tokens: 3_000,
            max_definitions: 24,
            keep_meta: false,
            load_mode: LoadMode::Indirect,
            visible_bound: None,
            direct_groups: vec![],
        }
    }
}

/// A classification of one set of candidate definitions. Order is the
/// input's; the manifest sorts.
#[derive(Debug, Default)]
pub struct Split {
    pub direct: Vec<ToolDefinition>,
    pub deferred: Vec<ToolDefinition>,
    /// The meta tools, present only when something is deferred.
    pub meta: Vec<ToolDefinition>,
    /// Direct tools moved to the deferred set because the visible bound left
    /// no room for them (in candidate order). Never silent: the host reports
    /// them.
    pub demoted: Vec<String>,
}

impl Classification {
    /// The classification a frozen model profile asks for: its direct tools
    /// (and, under `dialect`, the wire names they are offered as), its
    /// visible-tool bound and its load mode. The token budget stays the
    /// default.
    pub fn from_profile(profile: &AiModelToolProfile, dialect: Option<&Dialect>) -> Self {
        let mut names: BTreeSet<String> = BTreeSet::new();
        let mut groups = vec![];
        for name in &profile.direct_tools {
            let mut group = vec![name.clone()];
            if let Some(dialect) = dialect {
                group.extend(dialect.wire_names_of(name));
            }
            names.extend(group.iter().cloned());
            groups.push(group);
        }
        let bound = profile.max_visible_tools.max(0.0) as usize;
        Self {
            direct: DirectSet::Names(names),
            max_definitions: bound,
            visible_bound: Some(bound),
            direct_groups: groups,
            load_mode: match profile.load_mode {
                AiModelLoadMode::Promote => LoadMode::Promote,
                AiModelLoadMode::Indirect => LoadMode::Indirect,
            },
            ..Self::default()
        }
    }

    fn is_direct(&self, name: &str) -> bool {
        match &self.direct {
            DirectSet::Names(names) => names.contains(name),
            DirectSet::Unspecified => !self.deferred_hints.contains(name),
        }
    }

    pub fn split(&self, candidates: &[ToolDefinition]) -> Split {
        let (meta, rest): (Vec<_>, Vec<_>) = candidates
            .iter()
            .cloned()
            .partition(|definition| is_meta_tool(&definition.name));
        let kept_meta = if self.keep_meta { meta.len() } else { 0 };
        let within_bound = self
            .visible_bound
            .is_none_or(|bound| rest.len() + kept_meta <= bound);
        if rest.len() <= self.max_definitions
            && estimate_tokens(&rest) <= self.max_tokens
            && within_bound
        {
            return Split {
                direct: rest,
                meta: if self.keep_meta { meta } else { vec![] },
                ..Split::default()
            };
        }
        let (mut direct, mut deferred): (Vec<_>, Vec<_>) = rest
            .into_iter()
            .partition(|definition| self.is_direct(&definition.name));
        let demoted = self.demote_over_bound(&mut direct, &mut deferred, meta.len());
        let meta = if deferred.is_empty() && !self.keep_meta {
            vec![]
        } else {
            meta
        };
        Split {
            direct,
            deferred,
            meta,
            demoted,
        }
    }

    /// Holds the rendered array to the visible bound: the direct tools plus
    /// the meta tools (reserved once anything is deferred) fit, or the
    /// overflow moves to `deferred`. Whole tools go together (the wires of one
    /// canonical tool), last of the profile's priority order first.
    fn demote_over_bound(
        &self,
        direct: &mut Vec<ToolDefinition>,
        deferred: &mut Vec<ToolDefinition>,
        meta: usize,
    ) -> Vec<String> {
        let Some(bound) = self.visible_bound else {
            return vec![];
        };
        let meta_needed = !deferred.is_empty() || self.keep_meta;
        if direct.len() + if meta_needed { meta } else { 0 } <= bound {
            return vec![];
        }
        // Something is deferred from here on, so the meta tools take slots.
        let room = bound.saturating_sub(meta);
        let mut excess = direct.len().saturating_sub(room);
        let mut out: BTreeSet<String> = BTreeSet::new();
        let present: BTreeSet<&str> = direct.iter().map(|d| d.name.as_str()).collect();
        for group in self.direct_groups.iter().rev() {
            if excess == 0 {
                break;
            }
            let members: Vec<&String> = group
                .iter()
                .filter(|name| present.contains(name.as_str()) && !out.contains(*name))
                .collect();
            excess = excess.saturating_sub(members.len());
            out.extend(members.into_iter().cloned());
        }
        // Tools no group names go last-in-candidate-order first.
        for definition in direct.iter().rev() {
            if excess == 0 {
                break;
            }
            if out.insert(definition.name.clone()) {
                excess -= 1;
            }
        }
        let (gone, kept): (Vec<_>, Vec<_>) = std::mem::take(direct)
            .into_iter()
            .partition(|definition| out.contains(&definition.name));
        *direct = kept;
        let names = gone.iter().map(|d| d.name.clone()).collect();
        deferred.extend(gone);
        names
    }
}

/// Rough size of definitions as a model reads them (4 bytes a token).
pub fn estimate_tokens(definitions: &[ToolDefinition]) -> usize {
    definitions
        .iter()
        .map(|definition| serde_json::to_string(definition).map_or(0, |text| text.len()) / 4)
        .sum()
}

/// The toolset id of a tool nobody declared a toolset for: the part of the
/// name before the last `__` (`mcp__files__ls` is the dynamic toolset
/// `mcp__files` of a connected server), else the first segment of the
/// capability. Declared toolsets (T4.1) come first; see `ToolRegistry`.
pub fn namespace_of(definition: &ToolDefinition) -> String {
    match definition.name.rfind("__") {
        Some(at) if at > 0 => definition.name[..at].to_owned(),
        _ => definition
            .capability
            .split('.')
            .next()
            .unwrap_or_default()
            .to_owned(),
    }
}

/// One tool the session knows, direct or deferred.
#[derive(Clone, Debug)]
pub struct CatalogEntry {
    pub definition: ToolDefinition,
    pub deferred: bool,
    /// False when no contributor offers it this turn or a guard withheld it.
    pub available: bool,
    /// The toolset id (the catalog's namespace).
    pub namespace: String,
    /// The canonical tool id behind `definition.name` (differs when a
    /// dialect renames the tool).
    pub canonical: String,
}

/// The session's tools by name, filled once at freeze and read by the meta
/// tools.
#[derive(Clone, Debug, Default)]
pub struct Catalog {
    entries: Vec<CatalogEntry>,
    /// True when the session's array carries the meta tools, so a deferred
    /// tool can be run with `tool_call`.
    meta_active: bool,
    toolsets: ToolsetView,
}

pub type CatalogCell = Arc<OnceLock<Catalog>>;

impl Catalog {
    pub fn new(mut entries: Vec<CatalogEntry>, meta_active: bool) -> Self {
        entries.sort_by(|a, b| a.definition.name.cmp(&b.definition.name));
        Self {
            entries,
            meta_active,
            toolsets: ToolsetView::default(),
        }
    }

    /// This catalog plus `extra` entries (names it does not hold), sorted
    /// again. Used to show promoted tools that joined after the freeze.
    pub fn with_extra(&self, extra: Vec<CatalogEntry>) -> Self {
        let mut entries = self.entries.clone();
        entries.extend(extra);
        Self::new(entries, self.meta_active).with_toolsets(self.toolsets.clone())
    }

    /// The same catalog with the session's toolsets (guides and cards).
    pub fn with_toolsets(mut self, toolsets: ToolsetView) -> Self {
        self.toolsets = toolsets;
        self
    }

    pub fn toolsets(&self) -> &ToolsetView {
        &self.toolsets
    }

    pub fn entries(&self) -> &[CatalogEntry] {
        &self.entries
    }

    pub fn meta_active(&self) -> bool {
        self.meta_active
    }

    pub fn get(&self, name: &str) -> Option<&CatalogEntry> {
        self.entries
            .binary_search_by(|entry| entry.definition.name.as_str().cmp(name))
            .ok()
            .map(|at| &self.entries[at])
    }

    pub fn names(&self) -> Vec<String> {
        self.entries
            .iter()
            .map(|entry| entry.definition.name.clone())
            .collect()
    }

    pub fn deferred_len(&self) -> usize {
        self.entries.iter().filter(|entry| entry.deferred).count()
    }

    /// Lexical search over the deferred entries. A query word scores once, by
    /// the heaviest field it matches: name or namespace 3, parameter name 2,
    /// description 1. Ties break by name; no embeddings. Returns the best
    /// `limit` and how many matched in all.
    pub fn search(
        &self,
        query: &str,
        namespace: Option<&str>,
        limit: usize,
    ) -> (Vec<&CatalogEntry>, usize) {
        let words: BTreeSet<String> = tokens(query).into_iter().collect();
        let mut scored: Vec<(usize, &CatalogEntry)> = self
            .entries
            .iter()
            .filter(|entry| entry.deferred)
            .filter(|entry| namespace.is_none_or(|wanted| entry.namespace == wanted))
            .filter_map(|entry| {
                let score = score(entry, &words);
                (score > 0).then_some((score, entry))
            })
            .collect();
        scored.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| a.1.definition.name.cmp(&b.1.definition.name))
        });
        let total = scored.len();
        (
            scored.into_iter().take(limit).map(|(_, e)| e).collect(),
            total,
        )
    }
}

fn tokens(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(str::to_lowercase)
        .collect()
}

fn matches(word: &str, field: &BTreeSet<String>) -> bool {
    field
        .iter()
        .any(|token| token == word || (word.len() >= 3 && token.starts_with(word)))
}

fn score(entry: &CatalogEntry, words: &BTreeSet<String>) -> usize {
    let definition = &entry.definition;
    let heavy: BTreeSet<String> = tokens(&definition.name)
        .into_iter()
        .chain(tokens(&entry.namespace))
        .collect();
    let params: BTreeSet<String> = parameter_names(&definition.input_schema)
        .iter()
        .flat_map(|name| tokens(name))
        .collect();
    let light: BTreeSet<String> = tokens(&definition.description).into_iter().collect();
    words
        .iter()
        .map(|word| {
            if matches(word, &heavy) {
                3
            } else if matches(word, &params) {
                2
            } else {
                usize::from(matches(word, &light))
            }
        })
        .sum()
}

/// Top-level parameter names of a schema, in schema order, with `required`.
pub fn parameter_names(schema: &Value) -> Vec<String> {
    schema["properties"]
        .as_object()
        .map(|properties| properties.keys().cloned().collect())
        .unwrap_or_default()
}

pub fn required_names(schema: &Value) -> BTreeSet<String> {
    schema["required"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}

/// The first sentence of `text`, cut at `max` characters.
pub fn first_sentence(text: &str, max: usize) -> String {
    let text = text.trim();
    let end = text
        .char_indices()
        .find(|(at, c)| {
            matches!(c, '.' | '!' | '?') && text[at + 1..].starts_with(char::is_whitespace)
        })
        .map_or(text.len(), |(at, c)| at + c.len_utf8());
    let sentence = &text[..end];
    if sentence.chars().count() <= max {
        return sentence.to_owned();
    }
    let cut: String = sentence.chars().take(max.saturating_sub(1)).collect();
    format!("{}...", cut.trim_end())
}

const INDEX_NAMESPACES: usize = 24;
const PURPOSE_CHARS: usize = 100;

/// The S2 catalog index: one line per toolset with a deferred member or a
/// guide, sorted, built only from the definitions and the toolset cards, so
/// equal catalogs give equal bytes (cache-stable). `None` when no line
/// qualifies.
pub fn toolset_index(
    direct: &[ToolDefinition],
    deferred: &[ToolDefinition],
    view: &ToolsetView,
) -> Option<String> {
    #[derive(Default)]
    struct Group<'a> {
        direct: Vec<&'a ToolDefinition>,
        deferred: Vec<&'a ToolDefinition>,
    }
    let mut groups: std::collections::BTreeMap<String, Group<'_>> = Default::default();
    for definition in direct.iter().filter(|d| !is_meta_tool(&d.name)) {
        groups
            .entry(view.id_of(definition))
            .or_default()
            .direct
            .push(definition);
    }
    for definition in deferred {
        groups
            .entry(view.id_of(definition))
            .or_default()
            .deferred
            .push(definition);
    }
    groups.retain(|id, group| {
        !group.deferred.is_empty() || view.toolset(id).is_some_and(|set| set.guide.is_some())
    });
    if groups.is_empty() {
        return None;
    }
    let mut lines = vec![format!(
        "Deferred tools are not in your tool list. Find one with {TOOL_SEARCH}, read its schema with {TOOL_DESCRIBE}, run it with {TOOL_CALL}; {TOOLSET_LOAD} returns a toolset's guide and tool list. Toolsets:"
    )];
    let total = groups.len();
    for (id, mut group) in groups.into_iter().take(INDEX_NAMESPACES) {
        group.deferred.sort_by(|a, b| a.name.cmp(&b.name));
        let purpose = match view.toolset(&id) {
            Some(set) if !set.card.is_empty() => set.card.clone(),
            _ => first_sentence(
                group
                    .deferred
                    .first()
                    .map_or("", |d| d.description.as_str()),
                PURPOSE_CHARS,
            ),
        };
        lines.push(format!(
            "- {} ({}): {}",
            if id.is_empty() { "general" } else { &id },
            group.direct.len() + group.deferred.len(),
            purpose
        ));
    }
    if total > INDEX_NAMESPACES {
        lines.push(format!(
            "- and {} more toolsets: use {TOOL_SEARCH}.",
            total - INDEX_NAMESPACES
        ));
    }
    Some(lines.join("\n"))
}

/// [`toolset_index`] for deferred definitions alone, grouped by the stand-in
/// namespace rule.
pub fn catalog_index(deferred: &[ToolDefinition]) -> Option<String> {
    toolset_index(&[], deferred, &ToolsetView::default())
}
