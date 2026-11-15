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

use crate::ToolDefinition;
use serde_json::Value;
use std::collections::BTreeSet;
use std::sync::{Arc, OnceLock};

pub const TOOL_SEARCH: &str = "tool_search";
pub const TOOL_DESCRIBE: &str = "tool_describe";
pub const TOOL_CALL: &str = "tool_call";
/// The meta tools, in the order they are documented.
pub const META_TOOLS: [&str; 3] = [TOOL_SEARCH, TOOL_DESCRIBE, TOOL_CALL];

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

/// Everything the freeze needs to decide direct or deferred.
#[derive(Clone, Debug)]
pub struct Classification {
    pub direct: DirectSet,
    /// Tools whose own [`ToolLoading`] is `Deferred` (from the registry).
    pub deferred_hints: BTreeSet<String>,
    /// A manifest within both limits is all direct and needs no meta tools.
    pub max_tokens: usize,
    pub max_definitions: usize,
}

impl Default for Classification {
    fn default() -> Self {
        Self {
            direct: DirectSet::Unspecified,
            deferred_hints: BTreeSet::new(),
            max_tokens: 3_000,
            max_definitions: 24,
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
}

impl Classification {
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
        if rest.len() <= self.max_definitions && estimate_tokens(&rest) <= self.max_tokens {
            return Split {
                direct: rest,
                ..Split::default()
            };
        }
        let (direct, deferred): (Vec<_>, Vec<_>) = rest
            .into_iter()
            .partition(|definition| self.is_direct(&definition.name));
        let meta = if deferred.is_empty() { vec![] } else { meta };
        Split {
            direct,
            deferred,
            meta,
        }
    }
}

/// Rough size of definitions as a model reads them (4 bytes a token).
pub fn estimate_tokens(definitions: &[ToolDefinition]) -> usize {
    definitions
        .iter()
        .map(|definition| serde_json::to_string(definition).map_or(0, |text| text.len()) / 4)
        .sum()
}

/// A stand-in for the toolset until toolsets exist (T4.1): the part of the
/// name before the last `__` (`mcp__files__ls` is `mcp__files`), else the
/// first segment of the capability.
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
    pub namespace: String,
}

/// The session's tools by name, filled once at freeze and read by the meta
/// tools.
#[derive(Clone, Debug, Default)]
pub struct Catalog {
    entries: Vec<CatalogEntry>,
    /// True when the session's array carries the meta tools, so a deferred
    /// tool can be run with `tool_call`.
    meta_active: bool,
}

pub type CatalogCell = Arc<OnceLock<Catalog>>;

impl Catalog {
    pub fn new(mut entries: Vec<CatalogEntry>, meta_active: bool) -> Self {
        entries.sort_by(|a, b| a.definition.name.cmp(&b.definition.name));
        Self {
            entries,
            meta_active,
        }
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

/// The S2 catalog index: one line per deferred namespace, sorted, built only
/// from the definitions, so equal catalogs give equal bytes (cache-stable).
/// `None` when nothing is deferred.
pub fn catalog_index(deferred: &[ToolDefinition]) -> Option<String> {
    if deferred.is_empty() {
        return None;
    }
    let mut groups: std::collections::BTreeMap<String, Vec<&ToolDefinition>> = Default::default();
    for definition in deferred {
        groups
            .entry(namespace_of(definition))
            .or_default()
            .push(definition);
    }
    let mut lines = vec![format!(
        "Deferred tools are not in your tool list. Find one with {TOOL_SEARCH}, read its schema with {TOOL_DESCRIBE}, run it with {TOOL_CALL}. Namespaces:"
    )];
    let total = groups.len();
    for (namespace, mut members) in groups.into_iter().take(INDEX_NAMESPACES) {
        members.sort_by(|a, b| a.name.cmp(&b.name));
        lines.push(format!(
            "- {} ({}): {}",
            if namespace.is_empty() {
                "general"
            } else {
                &namespace
            },
            members.len(),
            first_sentence(&members[0].description, PURPOSE_CHARS)
        ));
    }
    if total > INDEX_NAMESPACES {
        lines.push(format!(
            "- and {} more namespaces: use {TOOL_SEARCH}.",
            total - INDEX_NAMESPACES
        ));
    }
    Some(lines.join("\n"))
}
