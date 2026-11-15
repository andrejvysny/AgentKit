//! The session's turn-context record (roadmap 1.2, ADR 0036): which skills
//! and template the history already carries, the last date sent, and the
//! delta notes waiting for the next turn. It lives in the chat metadata and is
//! written in the same transaction that persists the turn's context part, so a
//! crash never leaves a part without its record.

use crate::ChatResult;
use agentkit_store::SqliteStore;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// Chat metadata key holding the turn-context record.
pub const TURN_CONTEXT_METADATA_KEY: &str = "agentkit.session.turnContext";

/// What a delta note reports. The wire name is the `kind` attribute.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnNoteKind {
    ProjectRulesChanged,
    CoreMemoryChanged,
    ToolNamespaceAdded,
    ToolNamespaceRemoved,
    SkillDeactivated,
    SkillReactivated,
    TemplateDeactivated,
    TemplateReactivated,
    /// The T4.4 toolset recommendation line for this turn.
    ToolsetRecommendation,
}

impl TurnNoteKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ProjectRulesChanged => "project_rules_changed",
            Self::CoreMemoryChanged => "core_memory_changed",
            Self::ToolNamespaceAdded => "tool_namespace_added",
            Self::ToolNamespaceRemoved => "tool_namespace_removed",
            Self::SkillDeactivated => "skill_deactivated",
            Self::SkillReactivated => "skill_reactivated",
            Self::TemplateDeactivated => "template_deactivated",
            Self::TemplateReactivated => "template_reactivated",
            Self::ToolsetRecommendation => "toolset_recommendation",
        }
    }

    /// The attribute that names what the note is about.
    pub(crate) fn subject_attr(self) -> &'static str {
        match self {
            Self::ProjectRulesChanged => "file",
            Self::CoreMemoryChanged => "scope",
            Self::ToolNamespaceAdded | Self::ToolNamespaceRemoved => "namespace",
            Self::SkillDeactivated
            | Self::SkillReactivated
            | Self::TemplateDeactivated
            | Self::TemplateReactivated
            | Self::ToolsetRecommendation => "name",
        }
    }
}

/// One delta note for the next turn's context: the head of the prompt kept its
/// old bytes and this says what changed since.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnNote {
    pub kind: TurnNoteKind,
    /// A file, scope, namespace or skill name, depending on the kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    pub text: String,
}

impl TurnNote {
    pub fn new(kind: TurnNoteKind, subject: Option<&str>, text: &str) -> Self {
        Self {
            kind,
            subject: subject.map(str::to_owned),
            text: text.to_owned(),
        }
    }
}

/// A skill or template as the history carries it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Pinned {
    pub id: String,
    pub revision: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TurnState {
    /// Skill id to the revision whose text is already in the history.
    #[serde(default)]
    pub skills: BTreeMap<String, String>,
    /// Skills in `skills` that were deselected: their text stays in the
    /// history, but the model was told to stop following it.
    #[serde(default)]
    pub inactive: BTreeSet<String>,
    #[serde(default)]
    pub template: Option<Pinned>,
    #[serde(default)]
    pub template_inactive: bool,
    /// The last date a turn context carried (`YYYY-MM-DD`).
    #[serde(default)]
    pub date: Option<String>,
    /// Notes queued since the last turn context.
    #[serde(default)]
    pub notes: Vec<TurnNote>,
}

impl TurnState {
    /// The record in `metadata`; absent or unreadable reads as empty.
    pub(crate) fn read(metadata: &Value) -> Self {
        metadata
            .get(TURN_CONTEXT_METADATA_KEY)
            .and_then(|value| serde_json::from_value(value.clone()).ok())
            .unwrap_or_default()
    }

    pub(crate) fn write(&self, metadata: &Value) -> Value {
        let mut next = metadata.clone();
        next[TURN_CONTEXT_METADATA_KEY] = serde_json::to_value(self).expect("state serializes");
        next
    }
}

/// Notes a single turn context may carry; the rest are counted, not written.
pub(crate) const MAX_TURN_NOTES: usize = 16;

/// `notes` with one note per (kind, subject): a later note replaces an
/// earlier one and takes its place at the end.
pub(crate) fn coalesce(notes: Vec<TurnNote>) -> Vec<TurnNote> {
    let mut kept: Vec<TurnNote> = vec![];
    for note in notes {
        kept.retain(|old| old.kind != note.kind || old.subject != note.subject);
        kept.push(note);
    }
    kept
}

/// `metadata` with `note` queued for the next turn's context.
pub fn with_turn_note(metadata: &Value, note: TurnNote) -> Value {
    let mut state = TurnState::read(metadata);
    state.notes.push(note);
    state.notes = coalesce(std::mem::take(&mut state.notes));
    state.write(metadata)
}

/// `metadata` without the queued notes of `kind` (their content was adopted
/// into the frozen head, so the delta is moot).
pub fn without_turn_notes(metadata: &Value, kind: TurnNoteKind) -> Value {
    let mut state = TurnState::read(metadata);
    state.notes.retain(|note| note.kind != kind);
    state.write(metadata)
}

/// Queue `note` for the next turn of `chat_id`. The next turn that starts
/// renders it into its turn context and clears the queue; a turn already
/// running keeps the context it built. Hosts of the project-rules, core-memory
/// and tool-namespace watchers call this (roadmap 1.4 and 3.3).
pub async fn append_turn_note(
    store: &SqliteStore,
    chat_id: &str,
    note: TurnNote,
    now_ms: i64,
) -> ChatResult<()> {
    store
        .mutate_chat(chat_id, now_ms, move |chat| {
            chat.metadata = with_turn_note(&chat.metadata, note);
            Ok(())
        })
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_queued_note_replaces_the_older_one_for_the_same_subject() {
        let mut metadata = Value::Null;
        for text in ["one", "two"] {
            metadata = with_turn_note(
                &metadata,
                TurnNote::new(TurnNoteKind::ProjectRulesChanged, Some("AGENTS.md"), text),
            );
        }
        metadata = with_turn_note(
            &metadata,
            TurnNote::new(TurnNoteKind::ProjectRulesChanged, Some("CLAUDE.md"), "x"),
        );
        let notes = TurnState::read(&metadata).notes;
        assert_eq!(notes.len(), 2);
        assert_eq!(notes[0].text, "two");
    }
}
