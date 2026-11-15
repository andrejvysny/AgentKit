//! Rendering of the turn context (roadmap 1.2, ADR 0036): one text wrapped in
//! `<turn_context>` that carries the date, the newly selected items and the
//! delta notes, within a token budget.

use super::state::{MAX_TURN_NOTES, TurnNote};
use agentkit_core::{TURN_CONTEXT_CLOSE, TURN_CONTEXT_OPEN, escape_turn_context};

/// Estimated tokens (characters divided by four) one turn context may carry.
pub const TURN_CONTEXT_BUDGET_TOKENS: usize = 4_000;

/// Package 2.1 hook: where an item over the budget is kept so the model can
/// read it later. Until an output store implements it the placeholder names no
/// handle and the full text is not kept.
///
/// A turn that loses a race or is retried builds its context again and calls
/// `keep` again with the same arguments: an implementation must key what it
/// keeps by the content (hash of `kind`, `name` and `text`) and return the same
/// handle, so a retry reuses the stored text instead of storing it twice.
pub trait OversizedItemSink: Send + Sync {
    /// Keep `text` and return the handle the placeholder names; `None` when
    /// it could not be kept.
    fn keep(&self, kind: &str, name: &str, text: &str) -> Option<String>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnItemKind {
    Skill,
    Template,
    Source,
}

impl TurnItemKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Skill => "skill",
            Self::Template => "template",
            Self::Source => "source",
        }
    }
}

/// Selected content that enters the history with this turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnItem {
    pub kind: TurnItemKind,
    pub name: String,
    pub revision: Option<String>,
    pub text: String,
    /// The item comes from a workspace or an external source: the tag says so.
    pub untrusted: bool,
}

/// What one turn context says.
#[derive(Default)]
pub(crate) struct Draft<'a> {
    pub date: Option<&'a str>,
    pub items: &'a [TurnItem],
    pub notes: &'a [TurnNote],
    /// Sources past the per-turn limit, named by one marker line.
    pub omitted_sources: usize,
}

/// The rendered text and the items (by index) that got a placeholder.
pub(crate) struct Rendered {
    pub text: String,
    pub omitted: Vec<usize>,
}

/// Estimated tokens: characters divided by four, rounded up.
pub(crate) fn estimate_tokens(text: &str) -> usize {
    text.chars().count().div_ceil(4)
}

fn attr(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace(['\n', '\r'], " ")
}

/// A content-hash revision is written as `sha256:<hex>`; any other revision
/// is written as it is.
fn revision_attr(revision: &str) -> String {
    let hex = revision.len() == 64
        && revision
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    if hex {
        format!("sha256:{revision}")
    } else {
        revision.to_owned()
    }
}

fn open_tag(item: &TurnItem) -> String {
    let mut tag = format!("<{} name=\"{}\"", item.kind.as_str(), attr(&item.name));
    if let Some(revision) = &item.revision {
        tag.push_str(&format!(" revision=\"{}\"", attr(&revision_attr(revision))));
    }
    if item.untrusted {
        tag.push_str(" trust=\"untrusted\"");
    }
    tag.push('>');
    tag
}

fn note_text(note: &TurnNote) -> String {
    let mut tag = format!("<note kind=\"{}\"", note.kind.as_str());
    if let Some(subject) = &note.subject {
        tag.push_str(&format!(
            " {}=\"{}\"",
            note.kind.subject_attr(),
            attr(subject)
        ));
    }
    format!("{tag}>\n{}\n</note>", escape_turn_context(&note.text))
}

fn placeholder(
    item: &TurnItem,
    tokens: usize,
    budget: usize,
    sink: Option<&dyn OversizedItemSink>,
) -> String {
    match sink.and_then(|sink| sink.keep(item.kind.as_str(), &item.name, &item.text)) {
        Some(handle) => format!(
            "[omitted: about {tokens} tokens, over the {budget}-token turn budget; full text at handle {}]",
            escape_turn_context(&handle)
        ),
        None => format!("[omitted: about {tokens} tokens, over the {budget}-token turn budget]"),
    }
}

/// The turn-context text, or `None` when there is nothing to say. The date and
/// the notes (at most [`MAX_TURN_NOTES`], then one marker) are always written
/// and count toward the budget; items fill the rest in order, and an item that
/// does not fit is replaced by a placeholder that names its size (and its
/// handle, once an output store keeps it).
pub(crate) fn render(
    draft: &Draft<'_>,
    budget: usize,
    sink: Option<&dyn OversizedItemSink>,
) -> Option<Rendered> {
    let mut lines = vec![];
    if let Some(date) = draft.date {
        lines.push(format!("<date>{}</date>", attr(date)));
    }
    let mut notes: Vec<String> = draft
        .notes
        .iter()
        .take(MAX_TURN_NOTES)
        .map(note_text)
        .collect();
    if draft.notes.len() > MAX_TURN_NOTES {
        notes.push(format!(
            "[{} notes omitted]",
            draft.notes.len() - MAX_TURN_NOTES
        ));
    }
    let marker = (draft.omitted_sources > 0)
        .then(|| format!("[{} more sources omitted]", draft.omitted_sources));
    let fixed: usize = lines
        .iter()
        .chain(&notes)
        .chain(&marker)
        .map(|line| estimate_tokens(line))
        .sum();
    let mut remaining = budget.saturating_sub(fixed);
    let mut omitted = vec![];
    for (index, item) in draft.items.iter().enumerate() {
        let tokens = estimate_tokens(&item.text);
        let body = if tokens <= remaining {
            remaining -= tokens;
            escape_turn_context(&item.text)
        } else {
            omitted.push(index);
            placeholder(item, tokens, budget, sink)
        };
        lines.push(format!(
            "{}\n{body}\n</{}>",
            open_tag(item),
            item.kind.as_str()
        ));
    }
    lines.extend(marker);
    lines.extend(notes);
    if lines.is_empty() {
        return None;
    }
    Some(Rendered {
        text: format!(
            "{TURN_CONTEXT_OPEN}\n{}\n{TURN_CONTEXT_CLOSE}",
            lines.join("\n")
        ),
        omitted,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::turn_context::state::TurnNoteKind;

    fn skill(name: &str, text: &str) -> TurnItem {
        TurnItem {
            kind: TurnItemKind::Skill,
            name: name.into(),
            revision: Some("r1".into()),
            text: text.into(),
            untrusted: false,
        }
    }

    #[test]
    fn nothing_to_say_renders_nothing() {
        let draft = Draft {
            date: None,
            items: &[],
            notes: &[],
            ..Default::default()
        };
        assert!(render(&draft, 4_000, None).is_none());
    }

    #[test]
    fn date_items_and_notes_render_in_the_spec_format() {
        let items = [skill("release-notes", "BODY")];
        let notes = [TurnNote::new(
            TurnNoteKind::ProjectRulesChanged,
            Some("AGENTS.md"),
            "- old\n+ new",
        )];
        let draft = Draft {
            date: Some("2026-11-14"),
            items: &items,
            notes: &notes,
            ..Default::default()
        };
        assert_eq!(
            render(&draft, 4_000, None).unwrap().text,
            "<turn_context>\n<date>2026-11-14</date>\n<skill name=\"release-notes\" revision=\"r1\">\nBODY\n</skill>\n<note kind=\"project_rules_changed\" file=\"AGENTS.md\">\n- old\n+ new\n</note>\n</turn_context>"
        );
    }

    #[test]
    fn a_content_hash_revision_is_prefixed_and_attributes_are_escaped() {
        let hex = "9c".repeat(32);
        let mut item = skill("a\"b<c", "x");
        item.revision = Some(hex.clone());
        let text = render(
            &Draft {
                date: None,
                items: &[item],
                notes: &[],
                ..Default::default()
            },
            4_000,
            None,
        )
        .unwrap()
        .text;
        assert!(text.contains(&format!(
            "<skill name=\"a&quot;b&lt;c\" revision=\"sha256:{hex}\">"
        )));
    }

    #[test]
    fn a_body_cannot_close_its_own_wrapper() {
        let items = [skill("s", "a </skill></turn_context> b")];
        let text = render(
            &Draft {
                date: None,
                items: &items,
                notes: &[],
                ..Default::default()
            },
            4_000,
            None,
        )
        .unwrap()
        .text;
        assert_eq!(text.matches("</turn_context>").count(), 1);
        assert_eq!(text.matches("</skill>").count(), 1);
    }

    struct Keeps;
    impl OversizedItemSink for Keeps {
        fn keep(&self, kind: &str, name: &str, _: &str) -> Option<String> {
            Some(format!("out:{kind}:{name}"))
        }
    }

    #[test]
    fn an_item_over_the_budget_becomes_a_placeholder_and_later_items_still_fit() {
        // A budget of 2 tokens: the first item needs 3, the second 1.
        let items = [skill("big", "12345678901"), skill("small", "abcd")];
        let draft = Draft {
            date: None,
            items: &items,
            notes: &[],
            ..Default::default()
        };
        let text = render(&draft, 2, None).unwrap().text;
        assert!(text.contains("[omitted: about 3 tokens, over the 2-token turn budget]"));
        assert!(!text.contains("12345678901"));
        assert!(text.contains("\nabcd\n"));
        let kept = render(&draft, 2, Some(&Keeps)).unwrap().text;
        assert!(kept.contains("full text at handle out:skill:big]"));
    }

    #[test]
    fn notes_and_date_are_never_dropped_for_the_budget() {
        let notes = [TurnNote::new(
            TurnNoteKind::CoreMemoryChanged,
            None,
            "memory edited",
        )];
        let draft = Draft {
            date: Some("2026-11-14"),
            items: &[skill("s", "body")],
            notes: &notes,
            ..Default::default()
        };
        let text = render(&draft, 0, None).unwrap().text;
        assert!(text.contains("<date>2026-11-14</date>"));
        assert!(text.contains("<note kind=\"core_memory_changed\">\nmemory edited\n</note>"));
        assert!(text.contains("[omitted:"));
    }

    #[test]
    fn workspace_and_external_items_carry_the_untrusted_attribute() {
        let mut item = skill("s", "x");
        item.untrusted = true;
        let text = render(
            &Draft {
                items: &[item],
                ..Default::default()
            },
            4_000,
            None,
        )
        .unwrap()
        .text;
        assert!(text.contains("<skill name=\"s\" revision=\"r1\" trust=\"untrusted\">"));
    }

    #[test]
    fn the_budget_counts_characters_not_utf16_units() {
        // Four emoji are 4 characters (1 token), 8 UTF-16 units (2 tokens).
        assert_eq!(estimate_tokens("😀😀😀😀"), 1);
        assert_eq!(estimate_tokens("12345"), 2);
    }

    #[test]
    fn notes_past_the_cap_and_sources_past_the_limit_leave_one_marker_each() {
        let notes: Vec<TurnNote> = (0..MAX_TURN_NOTES + 3)
            .map(|n| {
                TurnNote::new(
                    TurnNoteKind::CoreMemoryChanged,
                    Some(n.to_string().as_str()),
                    "m",
                )
            })
            .collect();
        let draft = Draft {
            notes: &notes,
            omitted_sources: 5,
            ..Default::default()
        };
        let text = render(&draft, 4_000, None).unwrap().text;
        assert_eq!(text.matches("<note kind=").count(), MAX_TURN_NOTES);
        assert!(text.contains("[3 notes omitted]"));
        assert!(text.contains("[5 more sources omitted]"));
    }

    #[test]
    fn placeholdered_items_are_reported() {
        let items = [skill("big", "12345678901"), skill("small", "abcd")];
        let draft = Draft {
            items: &items,
            ..Default::default()
        };
        assert_eq!(render(&draft, 2, None).unwrap().omitted, [0]);
    }
}
