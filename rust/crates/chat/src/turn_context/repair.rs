//! What the request's own history carries (ADR 0036): the instruction a turn
//! may skip sending is the one a turn-context message of THAT history already
//! holds, not one a chat-level ledger remembers. Also the spill of oversized
//! items into the output store (ADR 0034) and the repair that re-sends a
//! selected instruction a rewrite of the same request removed.

use super::attach::{chosen, skill_item, turn_user_message, with_part};
use super::render::{Draft, OversizedItemSink, TURN_CONTEXT_BUDGET_TOKENS, TurnItem, TurnItemKind};
use super::render::{estimate_tokens, render};
use super::state::{Carried, Delta, Pinned};
use crate::{ChatError, ChatResult, ExecutionContext};
use agentkit_core::{ChatMessage, OutputMeta, OutputStore, content_for_wire};
use agentkit_sessions::MessageRecord;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

/// What the history ending at `user` (excluded) carries: the deltas of its
/// ancestors' turn-context messages, oldest first, leaving out what the newest
/// stored summary covers. A fork or an edit has a different ancestry, so it
/// carries what its own path carries; a message from before this change has no
/// delta and counts as carrying nothing (its instruction is sent once more).
pub(super) async fn effective(
    context: &ExecutionContext,
    chat_id: &str,
    user: &MessageRecord,
) -> ChatResult<Carried> {
    let records = context.store.messages(chat_id, true).await?;
    let summaries = context.store.list_context_summaries(chat_id).await?;
    Ok(carried_by_ancestry(&records, &user.id, &summaries))
}

fn carried_by_ancestry(records: &[MessageRecord], head: &str, summaries: &[Value]) -> Carried {
    let by_id: HashMap<&str, &MessageRecord> = records.iter().map(|m| (m.id.as_str(), m)).collect();
    let mut chain: Vec<&MessageRecord> = vec![];
    let mut cursor = by_id
        .get(head)
        .and_then(|message| message.parent_message_id.as_deref());
    while let Some(message) = cursor.and_then(|id| by_id.get(id)) {
        chain.push(message);
        if chain.len() > records.len() {
            break;
        }
        cursor = message.parent_message_id.as_deref();
    }
    chain.reverse();
    let covered = summaries.iter().find_map(|summary| {
        let through = summary["coversThroughMessageId"].as_str()?;
        chain
            .iter()
            .find(|message| message.id == through)
            .map(|message| message.order_key)
    });
    let mut carried = Carried::default();
    for message in &chain {
        if message.role == "user"
            && covered.is_none_or(|through| message.order_key > through)
            && let Some(delta) = Delta::read(&message.metadata)
        {
            carried.apply(&delta);
        }
    }
    carried
}

/// The handles of items that went to the output store, by (kind, name).
#[derive(Default)]
pub(super) struct Spilled(HashMap<(String, String), String>);

impl OversizedItemSink for Spilled {
    fn keep(&self, kind: &str, name: &str, _: &str) -> Option<String> {
        self.0.get(&(kind.to_owned(), name.to_owned())).cloned()
    }
}

/// Stores each of `items` under the chat's scope, so `read_output` reads it
/// back exactly. A store that refuses leaves the item without a handle (the
/// render then says it could not be supplied in full) and warns.
pub(super) async fn spill(
    context: &ExecutionContext,
    store: &Arc<dyn OutputStore>,
    chat_id: &str,
    items: &[&TurnItem],
) -> ChatResult<Spilled> {
    let mut spilled = Spilled::default();
    for item in items {
        let kind = item.kind.as_str();
        let meta = OutputMeta {
            session: chat_id.to_owned(),
            task: context.task.task_id.clone(),
            call_id: format!("turn-context:{kind}:{}", item.name),
            tool: "turn_context".into(),
            mime: "text/markdown".into(),
            exit_code: None,
        };
        match store.put(meta, item.text.clone().into_bytes()).await {
            Ok(handle) => {
                spilled
                    .0
                    .insert((kind.to_owned(), item.name.clone()), handle.to_string());
            }
            Err(_) => {
                context
                    .emit(
                        "run.warning",
                        serde_json::json!({"code": "turn_context_spill_failed", "message": format!("The output store did not keep the selected {kind} {}; the turn says it could not be supplied in full.", item.name)}),
                    )
                    .await?;
            }
        }
    }
    Ok(spilled)
}

/// The turn-context delta each stored user message carries, by id.
pub(crate) fn deltas_of(records: &[MessageRecord]) -> HashMap<String, Delta> {
    records
        .iter()
        .filter(|record| record.role == "user")
        .filter_map(|record| Some((record.id.clone(), Delta::read(&record.metadata)?)))
        .collect()
}

fn fits(text: &str) -> bool {
    estimate_tokens(text) <= TURN_CONTEXT_BUDGET_TOKENS
}

/// After the request's history was rewritten (a compaction summary, a trim),
/// re-sends the selected skills and template whose body `entries` no longer
/// hold: they go into the turn's own user message as one more turn-context
/// part, written like the first one. A history that still holds them (the
/// normal case) is left alone. `deltas` are the turn-context deltas of the
/// stored messages by id.
pub(crate) async fn repair(
    context: &ExecutionContext,
    chat_id: &str,
    anchor: &str,
    selected: &[ChatMessage],
    deltas: &HashMap<String, Delta>,
    entries: &mut [(ChatMessage, Option<String>)],
) -> ChatResult<()> {
    if selected.is_empty() {
        return Ok(());
    }
    let mut carried = Carried::default();
    for id in entries.iter().filter_map(|(_, id)| id.as_deref()) {
        if let Some(delta) = deltas.get(id) {
            carried.apply(delta);
        }
    }
    let chosen = chosen(&context.task.payload, selected);
    let mut items: Vec<TurnItem> = chosen
        .skills
        .iter()
        .filter(|(pin, text)| carried.skills.get(&pin.id) != Some(&pin.revision) && fits(text))
        .map(|(pin, text)| skill_item(pin, text, chosen.untrusted.contains(&pin.id)))
        .collect();
    if let Some((pin, text)) = &chosen.template
        && carried.template.as_ref() != Some(pin)
        && fits(text)
    {
        items.push(TurnItem {
            kind: TurnItemKind::Template,
            name: pin.id.clone(),
            revision: Some(pin.revision.clone()),
            text: text.clone(),
            untrusted: chosen.untrusted.contains(&pin.id),
        });
    }
    if items.is_empty() {
        return Ok(());
    }
    let Some(user) = turn_user_message(context, chat_id, anchor).await? else {
        return Ok(());
    };
    let Some(at) = entries
        .iter()
        .position(|(_, id)| id.as_deref() == Some(user.id.as_str()))
    else {
        return Ok(());
    };
    let draft = Draft {
        items: &items,
        ..Draft::default()
    };
    let Some(rendered) = render(&draft, TURN_CONTEXT_BUDGET_TOKENS, None) else {
        return Ok(());
    };
    let mut delta = Delta::default();
    for (index, item) in items.iter().enumerate() {
        let (true, Some(revision)) = (!rendered.omitted.contains(&index), &item.revision) else {
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
    let content = commit(context, chat_id, &user.id, &rendered.text, &delta).await?;
    entries[at].0.content = content_for_wire(&content);
    Ok(())
}

async fn commit(
    context: &ExecutionContext,
    chat_id: &str,
    user_id: &str,
    text: &str,
    delta: &Delta,
) -> ChatResult<Value> {
    let tx = context.store.begin_transaction().await?;
    let view = tx.view();
    let now = context.runtime.now_ms();
    view.verify_fence(&context.fence, now).await?;
    let message = view
        .get_message(user_id)
        .await?
        .filter(|message| message.chat_id == chat_id)
        .ok_or(ChatError::SessionDisappeared)?;
    let content = with_part(&message.content, text);
    view.update_message(
        user_id,
        Some(content.clone()),
        (!delta.is_empty()).then(|| delta.write_into(&message.metadata)),
        None,
        None,
        now,
    )
    .await?;
    view.verify_fence(&context.fence, context.runtime.now_ms())
        .await?;
    context.check_cancelled()?;
    tx.commit().await?;
    Ok(content)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn message(
        id: &str,
        role: &str,
        parent: Option<&str>,
        order: u64,
        delta: &Delta,
    ) -> MessageRecord {
        serde_json::from_value(json!({
            "id": id, "chatId": "c", "runId": null, "role": role, "content": "x",
            "parentMessageId": parent, "toolCallId": null, "modelResultJson": null,
            "metadata": delta.write_into(&json!({})), "orderKey": order, "depth": 0,
            "branchIndex": 0, "active": true,
            "createdAt": "2026-01-01T00:00:00.000Z"
        }))
        .unwrap()
    }
    fn with_skill(id: &str, revision: &str) -> Delta {
        let mut delta = Delta::default();
        delta.skills.insert(id.into(), revision.into());
        delta
    }

    #[test]
    fn a_skill_counts_only_while_its_message_is_in_the_ancestry_and_uncovered() {
        let none = Delta::default();
        let records = vec![
            message("u1", "user", None, 1, &with_skill("a", "r1")),
            message("a1", "assistant", Some("u1"), 2, &none),
            message("u2", "user", Some("a1"), 3, &none),
            message("a2", "assistant", Some("u2"), 4, &none),
            message("u3", "user", Some("a2"), 5, &none),
            // A sibling branch that selected b: not in u3's ancestry.
            message("u2b", "user", Some("a1"), 6, &with_skill("b", "r1")),
        ];
        let carried = carried_by_ancestry(&records, "u3", &[]);
        assert_eq!(carried.skills.get("a").map(String::as_str), Some("r1"));
        assert!(!carried.skills.contains_key("b"));
        let summary = json!({"coversThroughMessageId": "a1"});
        assert!(
            carried_by_ancestry(&records, "u3", &[summary])
                .skills
                .is_empty()
        );
        let elsewhere = json!({"coversThroughMessageId": "u2b"});
        assert!(
            carried_by_ancestry(&records, "u3", &[elsewhere])
                .skills
                .contains_key("a")
        );
        assert!(carried_by_ancestry(&records, "u1", &[]).skills.is_empty());
    }
}
