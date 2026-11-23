//! Where the turn being answered starts in the request (`RunOptions::
//! turn_start`). User messages stored after it, a steer or a correction
//! follow-up, replay as plain user messages: without this index the run
//! would take the last of them for a new turn.

use agentkit_core::ChatMessage;
use agentkit_store::MessageRecord;

/// The user message this turn answers: the parent of the anchor
/// placeholder, as the turn context reads it.
pub(super) fn user_id(records: &[MessageRecord], anchor: &str) -> Option<String> {
    let parent = records
        .iter()
        .find(|record| record.id == anchor)?
        .parent_message_id
        .as_deref()?;
    records
        .iter()
        .find(|record| record.id == parent && record.role == "user")
        .map(|record| record.id.clone())
}

/// How far from the end of `entries` the turn's user message sits. Counted
/// from the end because the request's leading system messages may still be
/// replaced after this, its tail not.
pub(super) fn from_end(
    entries: &[(ChatMessage, Option<String>)],
    user: Option<&str>,
) -> Option<usize> {
    let user = user?;
    let at = entries
        .iter()
        .position(|(_, id)| id.as_deref() == Some(user))?;
    Some(entries.len() - at)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::history::{HistoryPolicy, assemble_with_ids};
    use agentkit_store::ConversationTimestamp;
    use serde_json::{Value, json};

    fn record(id: &str, role: &str, order_key: u64, metadata: Value) -> MessageRecord {
        MessageRecord {
            id: id.into(),
            chat_id: "c1".into(),
            run_id: (id != "u1").then(|| "r1".to_owned()),
            role: role.into(),
            content: json!(id),
            parent_message_id: (id == "p").then(|| "u1".to_owned()),
            tool_call_id: (role == "tool").then(|| "call-1".to_owned()),
            tool_calls: (id == "a1")
                .then(|| json!([{"id": "call-1", "name": "read_file", "argumentsJson": "{}"}])),
            model_result_json: None,
            metadata,
            order_key,
            depth: 0,
            branch_index: 0,
            active: true,
            archived: false,
            created_at: ConversationTimestamp::from_epoch_millis(1_700_000_000_000).unwrap(),
        }
    }

    /// A later attempt of a turn whose run read a file, was steered and was
    /// asked for a correction (private mode keeps that follow-up).
    fn stored_turn() -> Vec<MessageRecord> {
        vec![
            record("u1", "user", 1, json!({})),
            record("p", "assistant", 2, json!({"placeholder": true})),
            record("a1", "assistant", 3, json!({"internal": true})),
            record(
                "t1",
                "tool",
                4,
                json!({"internal": true, "toolName": "read_file"}),
            ),
            record(
                "s1",
                "user",
                5,
                json!({"steering": true, "inputId": "st-1"}),
            ),
            record(
                "f1",
                "user",
                6,
                json!({"internal": true, "correctionPass": 1, "canonicalProviderTurn": true}),
            ),
        ]
    }

    #[test]
    fn a_persisted_steer_or_correction_follow_up_does_not_start_the_turn() {
        let records = stored_turn();
        let user = user_id(&records, "p");
        assert_eq!(user.as_deref(), Some("u1"));
        let layout = agentkit_core::PromptLayout::from_system(Some("system"));
        let entries = assemble_with_ids(
            records,
            usize::MAX,
            "p",
            &layout,
            &[],
            "r1",
            HistoryPolicy::Transcript,
        );
        let users: Vec<_> = entries
            .iter()
            .filter(|(message, _)| message.role == "user")
            .filter_map(|(_, id)| id.as_deref())
            .collect();
        assert_eq!(users, ["u1", "s1", "f1"], "both replay as user messages");
        let from_end = from_end(&entries, user.as_deref()).unwrap();
        // The request may get another system head; the index still names u1.
        let mut messages: Vec<ChatMessage> =
            entries.into_iter().map(|(message, _)| message).collect();
        messages.insert(0, ChatMessage::text("system", "rules"));
        let start = messages.len() - from_end;
        assert_eq!(messages[start].content, json!("u1"));
    }

    #[test]
    fn no_turn_start_when_the_anchor_has_no_user_parent() {
        let mut records = stored_turn();
        assert_eq!(user_id(&records, "missing"), None);
        records[1].parent_message_id = Some("a1".into());
        assert_eq!(user_id(&records, "p"), None);
        assert_eq!(from_end(&[], Some("u1")), None);
    }
}
