//! Builds the turn context once, when a turn starts, and persists it as a
//! part of the turn's user message (roadmap 1.2, ADR 0036). A turn that is
//! retried, resumed or replayed after a restart finds the part on the message
//! and keeps it byte for byte (INV-2, CE-03).

use super::render::{Draft, TURN_CONTEXT_BUDGET_TOKENS, TurnItem, TurnItemKind, render};
use super::state::{
    Carried, Delta, MAX_TURN_NOTES, Pinned, TurnNote, TurnNoteKind, TurnState, coalesce,
};
use crate::{AgentExecutorOptions, ChatError, ChatResult, ExecutionContext};
use agentkit_core::{ChatMessage, TURN_CONTEXT_PART_TYPE, ToolRegistry, turn_context_part};
use serde_json::{Value, json};
use std::collections::BTreeSet;

/// Selected sources one turn may attach.
const MAX_SOURCES: usize = 32;

/// The note a deselected skill leaves behind.
const SKILL_DEACTIVATED: &str = "This skill is no longer active. Its earlier text stays in the conversation; do not follow it from this turn on.";
const SKILL_REACTIVATED: &str =
    "This skill is active again. Follow its earlier text in the conversation.";
const TEMPLATE_DEACTIVATED: &str = "This template is no longer active. Its earlier text stays in the conversation; do not follow it from this turn on.";
const TEMPLATE_REACTIVATED: &str =
    "This template is active again. Follow its earlier text in the conversation.";

/// Origins whose text is not the user's own: the item is tagged untrusted.
fn untrusted_origin(origin: &Value) -> bool {
    matches!(origin.as_str(), Some("workspace" | "external"))
}

pub(crate) fn has_turn_context(content: &Value) -> bool {
    content.as_array().is_some_and(|parts| {
        parts
            .iter()
            .any(|part| part["type"] == TURN_CONTEXT_PART_TYPE)
    })
}

/// `content` with the turn-context part after what the user typed.
pub(super) fn with_part(content: &Value, text: &str) -> Value {
    let mut parts = match content {
        Value::String(typed) => vec![json!({"type": "text", "text": typed})],
        Value::Array(parts) => parts.clone(),
        _ => vec![],
    };
    parts.push(turn_context_part(text));
    Value::Array(parts)
}

/// Days since 1970-01-01 as a civil date (proleptic Gregorian, UTC).
fn civil_date(epoch_ms: i64) -> String {
    let z = epoch_ms.div_euclid(86_400_000) + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

#[derive(Clone, Default)]
pub(super) struct Chosen {
    pub(super) skills: Vec<(Pinned, String)>,
    pub(super) template: Option<(Pinned, String)>,
    /// Selected messages whose owners the snapshot does not name.
    pub(super) loose: Vec<String>,
    /// Ids of the skills and the template that come from a workspace or an
    /// external origin (`contentSnapshot.origins`, id to origin).
    pub(super) untrusted: BTreeSet<String>,
}

fn body(message: &ChatMessage) -> String {
    message.content.as_str().unwrap_or_default().to_owned()
}

/// What the frozen snapshot selected, with each item's text. The snapshot
/// holds the template's messages first, then one block per skill in
/// selection order (`resolve_run_content`).
pub(super) fn chosen(payload: &Value, selected: &[ChatMessage]) -> Chosen {
    let selection = payload["contentSnapshot"]["selection"].clone();
    let parsed = serde_json::from_value::<agentkit_sessions::RunContentSelection>(selection).ok();
    let Some(selection) = parsed.filter(|s| s.skills.len() <= selected.len()) else {
        return Chosen {
            loose: selected.iter().map(body).collect(),
            ..Chosen::default()
        };
    };
    let untrusted = payload["contentSnapshot"]["origins"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(_, origin)| untrusted_origin(origin))
        .map(|(id, _)| id.clone())
        .collect();
    let split = selected.len() - selection.skills.len();
    let skills = selection
        .skills
        .iter()
        .zip(&selected[split..])
        .map(|(skill, message)| {
            let pin = Pinned {
                id: skill.id.clone(),
                revision: skill.revision.clone(),
            };
            (pin, body(message))
        })
        .collect();
    let rendered: Vec<String> = selected[..split]
        .iter()
        .map(|message| format!("{}: {}", message.role, body(message)))
        .collect();
    let template = selection.template.map(|template| {
        let pin = Pinned {
            id: template.id,
            revision: template.revision,
        };
        (pin, rendered.join("\n\n"))
    });
    let loose = if template.is_some() { vec![] } else { rendered };
    Chosen {
        skills,
        template,
        loose,
        untrusted,
    }
}

/// Source snapshots the host attached to this turn: `sourceSnapshots` in the
/// task payload, each `{"name": ..., "text": ..., "origin"?: ...}`. The first
/// [`MAX_SOURCES`] are kept; the count of the rest is returned.
fn sources(payload: &Value) -> (Vec<TurnItem>, usize) {
    let mut items: Vec<TurnItem> = payload["sourceSnapshots"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|source| {
            Some(TurnItem {
                kind: TurnItemKind::Source,
                name: source["name"].as_str()?.to_owned(),
                revision: None,
                text: source["text"].as_str()?.to_owned(),
                untrusted: untrusted_origin(&source["origin"]),
            })
        })
        .collect();
    let omitted = items.len().saturating_sub(MAX_SOURCES);
    items.truncate(MAX_SOURCES);
    (items, omitted)
}

/// What the turn adds besides the selected skills and template.
#[derive(Clone, Default)]
pub(super) struct Extras {
    pub(super) sources: Vec<TurnItem>,
    pub(super) omitted_sources: usize,
    /// Notes this turn creates (the toolset recommendation), written after
    /// the queued ones.
    pub(super) notes: Vec<TurnNote>,
}

/// What a state entry reverts to when the item that would set it got only a
/// placeholder: the history does not carry that text.
enum Revert {
    Skill(String, Option<String>),
    Template(Option<Pinned>),
}

struct Plan {
    items: Vec<TurnItem>,
    /// Parallel to `items`.
    reverts: Vec<Option<Revert>>,
    notes: Vec<TurnNote>,
    date: Option<String>,
    omitted_sources: usize,
    next: TurnState,
}

impl Plan {
    fn push(&mut self, item: TurnItem, revert: Option<Revert>) {
        self.items.push(item);
        self.reverts.push(revert);
    }

    /// What the written message carries: the bodies that were sent (not the
    /// placeholders) and the (de)activation notes.
    fn delta(&self, omitted: &[usize]) -> Delta {
        let mut delta = Delta::default();
        for (index, item) in self.items.iter().enumerate() {
            let Some(revision) = item.revision.as_ref().filter(|_| !omitted.contains(&index))
            else {
                continue;
            };
            match item.kind {
                TurnItemKind::Skill => {
                    delta.skills.insert(item.name.clone(), revision.clone());
                }
                TurnItemKind::Template => {
                    delta.template = Some(Pinned {
                        id: item.name.clone(),
                        revision: revision.clone(),
                    });
                }
                TurnItemKind::Source => {}
            }
        }
        for note in self.notes.iter().take(MAX_TURN_NOTES) {
            let subject = note.subject.clone().unwrap_or_default();
            match note.kind {
                TurnNoteKind::SkillDeactivated => {
                    delta.deactivated.insert(subject);
                }
                TurnNoteKind::SkillReactivated => {
                    delta.reactivated.insert(subject);
                }
                TurnNoteKind::TemplateDeactivated => delta.template_deactivated = true,
                TurnNoteKind::TemplateReactivated => delta.template_reactivated = true,
                _ => {}
            }
        }
        delta
    }

    /// The state after rendering: an item that got a placeholder is not
    /// carried, so a later turn sends it again.
    fn settled(mut self, omitted: &[usize]) -> TurnState {
        for index in omitted {
            match self.reverts[*index].take() {
                Some(Revert::Skill(id, Some(old))) => {
                    self.next.skills.insert(id, old);
                }
                Some(Revert::Skill(id, None)) => {
                    self.next.skills.remove(&id);
                }
                Some(Revert::Template(old)) => self.next.template = old,
                None => {}
            }
        }
        self.next
    }
}

pub(super) fn skill_item(pin: &Pinned, text: &str, untrusted: bool) -> TurnItem {
    TurnItem {
        kind: TurnItemKind::Skill,
        name: pin.id.clone(),
        revision: Some(pin.revision.clone()),
        text: text.to_owned(),
        untrusted,
    }
}

/// What this turn must add to the history, given what the history it will be
/// sent with already `carried`.
fn plan(
    state: &TurnState,
    carried: &Carried,
    chosen: Chosen,
    extras: Extras,
    today: Option<&str>,
) -> Plan {
    let mut plan = Plan {
        items: vec![],
        reverts: vec![],
        notes: vec![],
        date: None,
        omitted_sources: extras.omitted_sources,
        next: TurnState {
            notes: vec![],
            skills: carried.skills.clone(),
            inactive: carried.inactive.clone(),
            template: carried.template.clone(),
            template_inactive: carried.template_inactive,
            ..state.clone()
        },
    };
    let mut notes = state.notes.clone();
    for (pin, text) in &chosen.skills {
        let held = carried.skills.get(&pin.id);
        plan.next
            .skills
            .insert(pin.id.clone(), pin.revision.clone());
        plan.next.inactive.remove(&pin.id);
        if held != Some(&pin.revision) {
            let untrusted = chosen.untrusted.contains(&pin.id);
            let revert = Revert::Skill(pin.id.clone(), held.cloned());
            plan.push(skill_item(pin, text, untrusted), Some(revert));
        } else if carried.inactive.contains(&pin.id) {
            notes.push(TurnNote::new(
                TurnNoteKind::SkillReactivated,
                Some(&pin.id),
                SKILL_REACTIVATED,
            ));
        }
    }
    for id in carried.skills.keys() {
        let selected = chosen.skills.iter().any(|(pin, _)| &pin.id == id);
        if !selected && plan.next.inactive.insert(id.clone()) {
            notes.push(TurnNote::new(
                TurnNoteKind::SkillDeactivated,
                Some(id),
                SKILL_DEACTIVATED,
            ));
        }
    }
    match (&chosen.template, &carried.template) {
        (Some((pin, _)), Some(held)) if held == pin => {
            plan.next.template_inactive = false;
            if carried.template_inactive {
                notes.push(TurnNote::new(
                    TurnNoteKind::TemplateReactivated,
                    Some(&pin.id),
                    TEMPLATE_REACTIVATED,
                ));
            }
        }
        (Some((pin, text)), held) => {
            plan.next.template = Some(pin.clone());
            plan.next.template_inactive = false;
            let item = TurnItem {
                kind: TurnItemKind::Template,
                name: pin.id.clone(),
                revision: Some(pin.revision.clone()),
                text: text.clone(),
                untrusted: chosen.untrusted.contains(&pin.id),
            };
            plan.push(item, Some(Revert::Template(held.clone())));
        }
        (None, Some(held)) if !carried.template_inactive => {
            plan.next.template_inactive = true;
            notes.push(TurnNote::new(
                TurnNoteKind::TemplateDeactivated,
                Some(&held.id),
                TEMPLATE_DEACTIVATED,
            ));
        }
        (None, _) => {}
    }
    for text in chosen.loose {
        plan.push(
            TurnItem {
                kind: TurnItemKind::Source,
                name: "selected-content".into(),
                revision: None,
                text,
                untrusted: false,
            },
            None,
        );
    }
    for source in extras.sources {
        plan.push(source, None);
    }
    notes.extend(extras.notes);
    plan.notes = coalesce(notes);
    plan.date = today
        .filter(|today| state.date.as_deref() != Some(*today))
        .map(str::to_owned);
    if plan.date.is_some() {
        plan.next.date = plan.date.clone();
    }
    plan
}

/// The user message the turn answers: the parent of its assistant placeholder.
pub(super) async fn turn_user_message(
    context: &ExecutionContext,
    chat_id: &str,
    anchor: &str,
) -> ChatResult<Option<agentkit_sessions::MessageRecord>> {
    let Some(parent) = context
        .store
        .get_message(anchor)
        .await?
        .and_then(|placeholder| placeholder.parent_message_id)
    else {
        return Ok(None);
    };
    Ok(context
        .store
        .get_message(&parent)
        .await?
        .filter(|user| user.role == "user" && user.chat_id == chat_id))
}

/// Builds the turn context of the turn answering `anchor` and persists it on
/// the user message, together with the updated record, in one transaction.
/// `selected` is the frozen selection, authorized for this dispatch. A user
/// message that already carries a turn-context part is left unchanged. The
/// toolset recommendation of the turn's hook (T4.4) becomes a note of the
/// context, so it is persisted once and replayed unchanged.
pub(crate) async fn attach(
    context: &ExecutionContext,
    options: &AgentExecutorOptions,
    registry: &ToolRegistry,
    chat_id: &str,
    anchor: &str,
    selected: &[ChatMessage],
) -> ChatResult<()> {
    let payload = &context.task.payload;
    let Some(user) = turn_user_message(context, chat_id, anchor).await? else {
        let has_sources = payload["sourceSnapshots"]
            .as_array()
            .is_some_and(|sources| !sources.is_empty());
        if !selected.is_empty() || has_sources {
            context.emit("run.warning", json!({"code":"turn_context_unattached","message":"The turn has no user message to carry the selected content; it was not sent to the model."})).await?;
        }
        return Ok(());
    };
    if has_turn_context(&user.content) {
        return Ok(());
    }
    let (sources, omitted_sources) = sources(payload);
    let notes = match &options.turn_context {
        Some(hook) => super::recommendation(hook.as_ref(), chat_id, registry, &user.content)
            .await
            .into_iter()
            .collect(),
        None => vec![],
    };
    let extras = Extras {
        sources,
        omitted_sources,
        notes,
    };
    let today = options
        .turn_context_date
        .then(|| civil_date(context.runtime.now_ms()));
    // What this turn's request will already hold, read from the history it is
    // built from; the chat-level record is only a cache.
    let carried = super::repair::effective(context, chat_id, &user).await?;
    let chosen = chosen(payload, selected);
    let spilled = spill_oversized(
        context,
        options,
        chat_id,
        (&carried, &chosen, &extras, today.as_deref()),
    )
    .await?;
    let sink: Option<&dyn super::OversizedItemSink> = match (&spilled, &options.turn_oversize) {
        (_, Some(sink)) => Some(sink.as_ref()),
        (Some(spilled), None) => Some(spilled),
        (None, None) => None,
    };
    persist(
        context,
        chat_id,
        &user.id,
        &carried,
        chosen,
        extras,
        (today.as_deref(), sink),
    )
    .await
}

/// Without a host sink, the items this turn would leave out for size go to
/// the output store, so `read_output` serves their handles. It runs before the
/// transaction opens (the store shares the database) on the plan the
/// transaction will most likely make again; an item it did not foresee gets
/// the explicit "could not be supplied in full" notice.
async fn spill_oversized(
    context: &ExecutionContext,
    options: &AgentExecutorOptions,
    chat_id: &str,
    (carried, chosen, extras, today): (&Carried, &Chosen, &Extras, Option<&str>),
) -> ChatResult<Option<super::repair::Spilled>> {
    let (None, Some(gate)) = (&options.turn_oversize, &options.output) else {
        return Ok(None);
    };
    let chat = context
        .store
        .get_chat(chat_id)
        .await?
        .ok_or(ChatError::SessionDisappeared)?;
    let state = TurnState::read(&chat.metadata);
    let dry = plan(&state, carried, chosen.clone(), extras.clone(), today);
    let draft = Draft {
        date: dry.date.as_deref(),
        items: &dry.items,
        notes: &dry.notes,
        omitted_sources: dry.omitted_sources,
    };
    let omitted = render(&draft, TURN_CONTEXT_BUDGET_TOKENS, None)
        .map(|rendered| rendered.omitted)
        .unwrap_or_default();
    let items: Vec<&TurnItem> = omitted.iter().map(|index| &dry.items[*index]).collect();
    Ok(Some(
        super::repair::spill(context, gate.store(), chat_id, &items).await?,
    ))
}

/// Plans, renders and commits in one transaction, from the record as the
/// transaction sees it: a note queued while this turn was starting is in that
/// record, is rendered, and is the one cleared. Nothing is read before.
async fn persist(
    context: &ExecutionContext,
    chat_id: &str,
    user_id: &str,
    carried: &Carried,
    chosen: Chosen,
    extras: Extras,
    (today, sink): (Option<&str>, Option<&dyn super::OversizedItemSink>),
) -> ChatResult<()> {
    let tx = context.store.begin_transaction().await?;
    let view = tx.view();
    let now = context.runtime.now_ms();
    view.verify_fence(&context.fence, now).await?;
    let chat = view
        .get_chat(chat_id)
        .await?
        .ok_or(ChatError::SessionDisappeared)?;
    let message = view
        .get_message(user_id)
        .await?
        .ok_or(ChatError::SessionDisappeared)?;
    // Another attempt got here first: its part is the one history keeps.
    if has_turn_context(&message.content) {
        return Ok(());
    }
    let state = TurnState::read(&chat.metadata);
    let plan = plan(&state, carried, chosen, extras, today);
    let draft = Draft {
        date: plan.date.as_deref(),
        items: &plan.items,
        notes: &plan.notes,
        omitted_sources: plan.omitted_sources,
    };
    let rendered = render(&draft, TURN_CONTEXT_BUDGET_TOKENS, sink);
    let omitted = rendered
        .as_ref()
        .map(|r| r.omitted.clone())
        .unwrap_or_default();
    let text = rendered.map(|r| r.text);
    let delta = plan.delta(&omitted);
    let next = plan.settled(&omitted);
    if text.is_none() && next == state {
        return Ok(());
    }
    if let Some(text) = &text {
        view.update_message(
            user_id,
            Some(with_part(&message.content, text)),
            (!delta.is_empty()).then(|| delta.write_into(&message.metadata)),
            None,
            None,
            now,
        )
        .await?;
    }
    view.update_chat(chat_id, None, Some(next.write(&chat.metadata)), None, now)
        .await?;
    view.verify_fence(&context.fence, context.runtime.now_ms())
        .await?;
    context.check_cancelled()?;
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(id: &str, revision: &str, text: &str) -> (Pinned, String) {
        (
            Pinned {
                id: id.into(),
                revision: revision.into(),
            },
            text.into(),
        )
    }
    fn chosen_skills(skills: Vec<(Pinned, String)>) -> Chosen {
        Chosen {
            skills,
            ..Chosen::default()
        }
    }
    fn plan_of(state: &TurnState, chosen: Chosen, today: Option<&str>) -> Plan {
        plan(state, &Carried::of(state), chosen, Extras::default(), today)
    }

    #[test]
    fn civil_dates_follow_the_gregorian_calendar() {
        assert_eq!(civil_date(0), "1970-01-01");
        assert_eq!(civil_date(951_782_400_000), "2000-02-29");
        assert_eq!(civil_date(1_794_700_800_000), "2026-11-15");
        assert_eq!(civil_date(-86_400_000), "1969-12-31");
    }

    #[test]
    fn a_new_skill_enters_once_and_a_repeat_adds_nothing() {
        let first = plan_of(
            &TurnState::default(),
            chosen_skills(vec![skill("a", "r1", "A BODY")]),
            None,
        );
        assert_eq!(first.items.len(), 1);
        assert_eq!(first.next.skills["a"], "r1");
        let again = plan_of(
            &first.next,
            chosen_skills(vec![skill("a", "r1", "A BODY")]),
            None,
        );
        assert!(again.items.is_empty() && again.notes.is_empty());
        assert_eq!(again.next, first.next);
        let revised = plan_of(
            &first.next,
            chosen_skills(vec![skill("a", "r2", "A BODY 2")]),
            None,
        );
        assert_eq!(revised.items.len(), 1);
    }

    #[test]
    fn deselecting_a_skill_keeps_its_pin_inactive_and_notes_it_once() {
        let held = plan_of(
            &TurnState::default(),
            chosen_skills(vec![skill("a", "r1", "A"), skill("b", "r1", "B")]),
            None,
        );
        let dropped = plan_of(&held.next, chosen_skills(vec![skill("b", "r1", "B")]), None);
        assert!(dropped.items.is_empty());
        assert_eq!(dropped.notes.len(), 1);
        assert_eq!(dropped.notes[0].kind, TurnNoteKind::SkillDeactivated);
        assert_eq!(dropped.notes[0].subject.as_deref(), Some("a"));
        assert_eq!(dropped.next.skills["a"], "r1");
        assert!(dropped.next.inactive.contains("a"));
        let later = plan_of(
            &dropped.next,
            chosen_skills(vec![skill("b", "r1", "B")]),
            None,
        );
        assert!(later.notes.is_empty());
    }

    #[test]
    fn reselecting_the_same_revision_sends_a_note_not_the_body() {
        let held = plan_of(
            &TurnState::default(),
            chosen_skills(vec![skill("a", "r1", "A")]),
            None,
        );
        let dropped = plan_of(&held.next, chosen_skills(vec![]), None);
        let back = plan_of(
            &dropped.next,
            chosen_skills(vec![skill("a", "r1", "A")]),
            None,
        );
        assert!(back.items.is_empty());
        assert_eq!(back.notes.len(), 1);
        assert_eq!(back.notes[0].kind, TurnNoteKind::SkillReactivated);
        assert!(back.next.inactive.is_empty());
        let revised = plan_of(
            &dropped.next,
            chosen_skills(vec![skill("a", "r2", "A2")]),
            None,
        );
        assert_eq!(revised.items.len(), 1);
        assert!(revised.notes.is_empty() && revised.next.inactive.is_empty());
    }

    fn with_template(revision: &str) -> Chosen {
        Chosen {
            template: Some((
                Pinned {
                    id: "t".into(),
                    revision: revision.into(),
                },
                "T".into(),
            )),
            ..Chosen::default()
        }
    }

    #[test]
    fn a_template_is_deactivated_and_reactivated_by_note() {
        let held = plan_of(&TurnState::default(), with_template("1"), None);
        assert_eq!(held.items.len(), 1);
        let dropped = plan_of(&held.next, Chosen::default(), None);
        assert_eq!(dropped.notes[0].kind, TurnNoteKind::TemplateDeactivated);
        assert!(dropped.next.template_inactive && dropped.next.template.is_some());
        let again = plan_of(&dropped.next, Chosen::default(), None);
        assert!(again.notes.is_empty());
        let back = plan_of(&dropped.next, with_template("1"), None);
        assert!(back.items.is_empty());
        assert_eq!(back.notes[0].kind, TurnNoteKind::TemplateReactivated);
        assert!(!back.next.template_inactive);
    }

    #[test]
    fn an_item_that_got_only_a_placeholder_is_not_recorded_as_carried() {
        let first = plan_of(
            &TurnState::default(),
            chosen_skills(vec![skill("a", "r1", "A")]),
            None,
        );
        assert!(first.settled(&[0]).skills.is_empty());
        let held = plan_of(
            &TurnState::default(),
            chosen_skills(vec![skill("a", "r1", "A")]),
            None,
        )
        .next;
        let revised = plan_of(&held, chosen_skills(vec![skill("a", "r2", "A2")]), None);
        assert_eq!(revised.settled(&[0]).skills["a"], "r1");
        let template = plan_of(&TurnState::default(), with_template("1"), None);
        assert!(template.settled(&[0]).template.is_none());
    }

    #[test]
    fn queued_notes_drain_and_the_date_repeats_only_when_it_changes() {
        let mut state = TurnState::default();
        state.notes.push(TurnNote::new(
            TurnNoteKind::CoreMemoryChanged,
            None,
            "edited",
        ));
        let first = plan_of(&state, chosen_skills(vec![]), Some("2026-11-14"));
        assert_eq!(first.date.as_deref(), Some("2026-11-14"));
        assert_eq!(first.notes.len(), 1);
        assert!(first.next.notes.is_empty());
        let same_day = plan_of(&first.next, chosen_skills(vec![]), Some("2026-11-14"));
        assert_eq!(same_day.date, None);
        let next_day = plan_of(&first.next, chosen_skills(vec![]), Some("2026-11-15"));
        assert_eq!(next_day.date.as_deref(), Some("2026-11-15"));
        let off = plan_of(&first.next, chosen_skills(vec![]), None);
        assert_eq!(off.date, None);
        assert_eq!(off.next.date.as_deref(), Some("2026-11-14"));
    }

    #[test]
    fn a_string_message_becomes_parts_and_the_part_is_found() {
        let content = with_part(&json!("hello"), "<turn_context>\n</turn_context>");
        assert_eq!(content[0], json!({"type": "text", "text": "hello"}));
        assert_eq!(content[1]["type"], "turn_context");
        assert!(has_turn_context(&content));
        assert!(!has_turn_context(&json!("hello")));
    }

    #[test]
    fn the_snapshot_splits_template_messages_from_skill_blocks() {
        let payload = json!({"contentSnapshot": {"selection": {
            "template": {"id": "t", "revision": "1", "arguments": {}},
            "skills": [{"id": "a", "revision": "r1"}]
        }, "origins": {"a": "workspace", "t": "builtin"}}});
        let selected = vec![
            ChatMessage::text("user", "template text"),
            ChatMessage::text("system", "skill text"),
        ];
        let picked = chosen(&payload, &selected);
        assert_eq!(picked.skills.len(), 1);
        assert_eq!(picked.skills[0].1, "skill text");
        assert_eq!(picked.template.as_ref().unwrap().1, "user: template text");
        assert!(picked.loose.is_empty());
        assert!(picked.untrusted.contains("a") && !picked.untrusted.contains("t"));
    }

    #[test]
    fn a_selection_the_snapshot_does_not_describe_is_kept_as_loose_text() {
        let picked = chosen(&json!({}), &[ChatMessage::text("system", "orphan")]);
        assert!(picked.skills.is_empty() && picked.template.is_none());
        assert_eq!(picked.loose, ["orphan"]);
    }

    #[test]
    fn sources_past_the_limit_are_counted_and_origins_mark_trust() {
        let many: Vec<Value> = (0..MAX_SOURCES + 4)
            .map(|n| json!({"name": format!("s{n}"), "text": "t", "origin": if n == 0 { "external" } else { "user" }}))
            .collect();
        let (items, omitted) = sources(&json!({"sourceSnapshots": many}));
        assert_eq!(items.len(), MAX_SOURCES);
        assert_eq!(omitted, 4);
        assert!(items[0].untrusted && !items[1].untrusted);
    }
}
