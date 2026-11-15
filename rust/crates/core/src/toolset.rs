//! Toolsets (native tool use T4.1, ADR 0033): the unit a model loads.
//!
//! A [`Toolset`] groups the tools of one area (`files`, `shell`, `git`, ...)
//! with a one-line *card* for the system prompt and a versioned *guide* the
//! model reads when it loads the set with `toolset_load`. Guides name other
//! tools through `{{tool:<canonical id>}}` placeholders that
//! [`render_guide`] fills with the names the session's dialect offers.
//! A connected server is a dynamic toolset (`mcp__alias`) with no guide.

use crate::deferred::{TOOL_CALL, TOOL_DESCRIBE, TOOL_SEARCH, TOOLSET_LOAD, namespace_of};
use crate::{CoreError, ToolDefinition};
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

pub const META_TOOLSET: &str = "meta";

/// A guide: versioned text, at most about 600 tokens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Guide {
    pub version: String,
    pub text: String,
}

impl Guide {
    /// Reads a guide file: optional `---` front matter with a `version:`
    /// line, then the text. A file without front matter has version "".
    pub fn parse(source: &str) -> Self {
        let (version, body) = source
            .strip_prefix("---\n")
            .and_then(|rest| rest.split_once("\n---\n"))
            .map_or((String::new(), source), |(front, body)| {
                let version = front
                    .lines()
                    .find_map(|line| line.strip_prefix("version:"))
                    .map(|value| value.trim().to_owned())
                    .unwrap_or_default();
                (version, body)
            });
        Self {
            version,
            text: body.trim().to_owned(),
        }
    }

    /// Rough size as a model reads it (4 bytes a token).
    pub fn estimated_tokens(&self) -> usize {
        self.text.len() / 4
    }
}

#[derive(Clone, Debug)]
pub struct Toolset {
    pub id: String,
    pub title: String,
    /// One line, about 20 tokens, shown in the system prompt.
    pub card: String,
    pub guide: Option<Guide>,
    /// Sub-guides by topic.
    pub topics: Vec<(String, Guide)>,
    /// Canonical ids of the member tools.
    pub tools: Vec<String>,
}

impl Toolset {
    pub fn new(id: &str, title: &str, card: &str) -> Self {
        Self {
            id: id.to_owned(),
            title: title.to_owned(),
            card: card.to_owned(),
            guide: None,
            topics: vec![],
            tools: vec![],
        }
    }

    pub fn with_guide(mut self, source: &str) -> Self {
        self.guide = Some(Guide::parse(source));
        self
    }

    pub fn with_topic(mut self, topic: &str, source: &str) -> Self {
        self.topics.push((topic.to_owned(), Guide::parse(source)));
        self
    }

    pub fn with_tools(mut self, tools: &[&str]) -> Self {
        self.tools = tools.iter().map(|tool| (*tool).to_owned()).collect();
        self
    }

    pub fn topic(&self, topic: &str) -> Option<&Guide> {
        self.topics
            .iter()
            .find(|(name, _)| name == topic)
            .map(|(_, guide)| guide)
    }
}

/// The meta tools as a toolset; every session has it.
pub fn meta_toolset() -> Toolset {
    Toolset::new(
        META_TOOLSET,
        "Meta tools",
        "Find, describe and run tools, and load toolsets.",
    )
    .with_tools(&[TOOL_SEARCH, TOOL_DESCRIBE, TOOL_CALL, TOOLSET_LOAD])
}

/// The session's toolsets and the toolset of each tool name the model is
/// offered. Frozen with the catalog, so the index and `toolset_load` read the
/// same answer all session.
#[derive(Clone, Debug, Default)]
pub struct ToolsetView {
    toolsets: BTreeMap<String, Toolset>,
    /// Offered tool name -> toolset id.
    names: BTreeMap<String, String>,
}

impl ToolsetView {
    pub fn new(toolsets: BTreeMap<String, Toolset>, names: BTreeMap<String, String>) -> Self {
        Self { toolsets, names }
    }

    pub fn toolset(&self, id: &str) -> Option<&Toolset> {
        self.toolsets.get(id)
    }

    pub fn toolsets(&self) -> impl Iterator<Item = &Toolset> {
        self.toolsets.values()
    }

    /// The toolset of an offered definition: the recorded one, else the
    /// dynamic toolset its name or capability implies.
    pub fn id_of(&self, definition: &ToolDefinition) -> String {
        self.names
            .get(&definition.name)
            .cloned()
            .unwrap_or_else(|| namespace_of(definition))
    }
}

/// Fills `{{tool:<canonical id>}}` with `shown[id]`; an id with no entry is
/// written as itself, so a guide never shows a placeholder.
pub fn render_guide(text: &str, shown: &BTreeMap<String, String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("{{tool:") {
        out.push_str(&rest[..start]);
        let after = &rest[start + "{{tool:".len()..];
        match after.find("}}") {
            Some(end) => {
                let id = after[..end].trim();
                out.push_str(shown.get(id).map_or(id, String::as_str));
                rest = &after[end + 2..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// The canonical ids a guide names through placeholders.
pub fn guide_placeholders(text: &str) -> Vec<String> {
    let mut ids = vec![];
    let mut rest = text;
    while let Some(start) = rest.find("{{tool:") {
        let after = &rest[start + "{{tool:".len()..];
        let Some(end) = after.find("}}") else { break };
        ids.push(after[..end].trim().to_owned());
        rest = &after[end + 2..];
    }
    ids
}

/// A toolset promoted into the tools array at a turn boundary.
#[derive(Clone, Debug, PartialEq)]
pub struct Promotion {
    pub toolset: String,
    /// The offered names of the tools that joined.
    pub tools: Vec<String>,
    /// The prefix revision recorded for the change, when the host records one.
    pub revision: Option<u64>,
}

/// The host side of a promotion: record it (re-stamp the session manifest)
/// before the next request is built. Returns the revision it recorded.
#[async_trait]
pub trait PromotionObserver: Send + Sync {
    async fn promoted(
        &self,
        toolset: &str,
        definitions: &[ToolDefinition],
    ) -> Result<Option<u64>, CoreError>;
}

#[derive(Default)]
struct PromotionState {
    pending: Vec<String>,
    applied: Vec<(String, Vec<ToolDefinition>)>,
    /// The tools the array may still take (the profile's visible bound minus
    /// what the frozen array holds); `None` is unbounded.
    capacity: Option<usize>,
    /// Promotions the bound refused at the turn boundary, to be reported.
    refused: Vec<(String, Vec<String>)>,
}

/// Toolsets asked to be promoted (by `toolset_load` in promote mode) and the
/// ones that were. Shared between the tool and the registry.
#[derive(Default)]
pub struct Promotions {
    state: Mutex<PromotionState>,
}

pub type PromotionCell = Arc<Promotions>;

impl Promotions {
    fn state(&self) -> std::sync::MutexGuard<'_, PromotionState> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Ask for `toolset` at the next turn boundary. False when it is already
    /// pending or promoted.
    pub fn request(&self, toolset: &str) -> bool {
        let mut state = self.state();
        let known = state.pending.iter().any(|id| id == toolset)
            || state.applied.iter().any(|(id, _)| id == toolset);
        if !known {
            state.pending.push(toolset.to_owned());
        }
        !known
    }

    /// Bound the tools promotion may add to the array.
    pub(crate) fn set_capacity(&self, capacity: Option<usize>) {
        self.state().capacity = capacity;
    }

    /// The tools promotion may still add, or `None` when unbounded.
    pub fn room(&self) -> Option<usize> {
        let state = self.state();
        let used: usize = state.applied.iter().map(|(_, defs)| defs.len()).sum();
        state.capacity.map(|capacity| capacity.saturating_sub(used))
    }

    /// True when `count` more tools fit the array now.
    pub fn fits(&self, count: usize) -> bool {
        self.room().is_none_or(|room| count <= room)
    }

    pub(crate) fn refuse(&self, toolset: &str, tools: Vec<String>) {
        self.state().refused.push((toolset.to_owned(), tools));
    }

    pub(crate) fn take_refused(&self) -> Vec<(String, Vec<String>)> {
        std::mem::take(&mut self.state().refused)
    }

    pub(crate) fn take_pending(&self) -> Vec<String> {
        std::mem::take(&mut self.state().pending)
    }

    pub(crate) fn apply(&self, toolset: &str, definitions: Vec<ToolDefinition>) {
        self.state().applied.push((toolset.to_owned(), definitions));
    }

    /// Definitions promoted so far, in promotion order.
    pub fn promoted_definitions(&self) -> Vec<ToolDefinition> {
        self.state()
            .applied
            .iter()
            .flat_map(|(_, definitions)| definitions.iter().cloned())
            .collect()
    }

    pub fn is_promoted_name(&self, name: &str) -> bool {
        self.state()
            .applied
            .iter()
            .any(|(_, definitions)| definitions.iter().any(|d| d.name == name))
    }
}
