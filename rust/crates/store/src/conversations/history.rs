use super::*;
use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug)]
pub struct ImportConversation {
    pub chat: ChatRecord,
    /// Creation order; all parents must precede their children.
    pub messages: Vec<MessageRecord>,
}

impl SqliteStore {
    pub async fn import_conversation(&self, mut input: ImportConversation) -> Result<ChatRecord> {
        self.call(move |db| {
            let transaction = crate::transaction::atomic(db)?;
            if get::<ChatRecord>(&transaction, "chats", "id", &input.chat.id)?.is_some() {
                return Err(invalid_import("duplicate_chat"));
            }
            validate_import(&mut input)?;
            input.chat.updated_at = input.chat.created_at.clone();
            transaction.execute(
                "INSERT INTO chats VALUES(?,?)",
                params![input.chat.id, json(&transaction, &input.chat)?],
            )?;
            for message in &input.messages {
                if get::<MessageRecord>(&transaction, "messages", "id", &message.id)?.is_some() {
                    return Err(StoreError::Duplicate(message.id.clone()));
                }
                transaction.execute(
                    "INSERT INTO messages VALUES(?,?,?,?,?)",
                    params![
                        message.id,
                        message.chat_id,
                        message.parent_message_id,
                        crate::util::sql_integer(message.order_key)?,
                        json(&transaction, message)?
                    ],
                )?;
            }
            transaction.commit()?;
            Ok(input.chat)
        })
        .await
    }

    pub async fn fork_chat(
        &self,
        chat_id: &str,
        from_message_id: &str,
        new_chat_id: &str,
        now: i64,
    ) -> Result<(ChatRecord, Vec<MessageRecord>)> {
        let (chat_id, from_message_id, new_chat_id) = (
            chat_id.to_owned(),
            from_message_id.to_owned(),
            new_chat_id.to_owned(),
        );
        self.call(move |db| {
            let transaction = crate::transaction::atomic(db)?;
            let source: ChatRecord = required(&transaction, "chats", "id", &chat_id)?;
            let mut cursor: MessageRecord =
                get::<MessageRecord>(&transaction, "messages", "id", &from_message_id)?
                    .filter(|message| message.chat_id == chat_id && message.active)
                    .ok_or_else(|| StoreError::InvalidInput("invalid_fork_point".into()))?;
            let mut path = vec![cursor.clone()];
            while let Some(parent) = cursor.parent_message_id {
                cursor = required(&transaction, "messages", "id", &parent)?;
                path.push(cursor.clone());
            }
            path.reverse();
            let path = order_messages_for_provider(
                path.into_iter()
                    .filter(|m| m.metadata.get("placeholder") != Some(&Value::Bool(true)))
                    .collect(),
            );
            let chat = ChatRecord {
                id: new_chat_id,
                title: source.title.map(|title| format!("Fork of {title}")),
                metadata: source.metadata,
                archived: false,
                created_at: ConversationTimestamp::from_epoch_millis(now)?,
                updated_at: ConversationTimestamp::from_epoch_millis(now)?,
            };
            transaction.execute(
                "INSERT INTO chats VALUES(?,?)",
                params![chat.id, json(&transaction, &chat)?],
            )?;
            let mut copied = Vec::new();
            for mut original in path {
                if let Some(metadata) = original.metadata.as_object_mut() {
                    metadata.remove("placeholder");
                }
                let message = append(
                    &transaction,
                    MessageInput {
                        chat_id: chat.id.clone(),
                        run_id: None,
                        role: original.role,
                        content: original.content,
                        tool_call_id: original.tool_call_id,
                        tool_calls: original.tool_calls,
                        model_result_json: original.model_result_json,
                        metadata: original.metadata,
                        ..Default::default()
                    },
                    now,
                )?;
                copied.push(message);
            }
            transaction.commit()?;
            Ok((chat, copied))
        })
        .await
    }

    /// Delete a whole chat scope atomically; all work must be cancelled or settled first.
    pub async fn delete_chat(&self, chat_id: &str) -> Result<()> {
        let chat_id = chat_id.to_owned();
        self.call(move |db| {
            let transaction = crate::transaction::atomic(db)?;
            let _: ChatRecord = required(&transaction, "chats", "id", &chat_id)?;
            let tasks: Vec<TaskRecord> = all::<TaskRecord>(&transaction, "tasks")?
                .into_iter()
                .filter(|t| t.scope_id == chat_id)
                .collect();
            if tasks.iter().any(|task| !task.status.is_terminal()) {
                return Err(StoreError::ScopeBusy(chat_id));
            }
            for proposal in all::<ProposalRecord>(&transaction, "proposals")?
                .into_iter()
                .filter(|p| p.chat_id == chat_id)
            {
                transaction.execute("DELETE FROM proposals WHERE id=?", [proposal.id])?;
            }
            for task in tasks {
                for outbox in all::<OutboxRecord>(&transaction, "outbox")?
                    .into_iter()
                    .filter(|o| o.run_id.as_deref() == Some(&task.task_id))
                {
                    transaction.execute("DELETE FROM outbox WHERE id=?", [outbox.id])?;
                }
                transaction.execute("DELETE FROM tasks WHERE id=?", [task.task_id])?;
            }
            transaction.execute("DELETE FROM chats WHERE id=?", [chat_id])?;
            transaction.commit()?;
            Ok(())
        })
        .await
    }

    pub async fn update_message(
        &self,
        message_id: &str,
        content: Option<Value>,
        metadata: Option<Value>,
        proof: Option<LeaseFence>,
        now: i64,
    ) -> Result<MessageRecord> {
        let message_id = message_id.to_owned();
        self.call(move |db| {
            let transaction = crate::transaction::atomic(db)?;
            let mut message: MessageRecord = required(&transaction, "messages", "id", &message_id)?;
            if let Some(proof) = proof {
                crate::tasks::fence(&transaction, &proof, now)?;
                if message.run_id.as_deref() != Some(&proof.task_id) {
                    return Err(StoreError::InvalidInput(
                        "message run does not match lease task".into(),
                    ));
                }
            }
            if let Some(content) = content {
                message.content = content;
            }
            if let Some(metadata) = metadata {
                message.metadata = metadata;
            }
            save_message(&transaction, &message)?;
            transaction.commit()?;
            Ok(message)
        })
        .await
    }
}

fn validate_import(input: &mut ImportConversation) -> Result<()> {
    input.chat.created_at.wire_text()?;
    let declared: HashSet<String> = input
        .messages
        .iter()
        .map(|message| message.id.clone())
        .collect();
    let mut seen = HashMap::<String, u32>::new();
    let mut children = HashSet::new();
    let mut siblings = HashMap::<Option<String>, u32>::new();
    for (index, message) in input.messages.iter_mut().enumerate() {
        message.created_at.wire_text()?;
        if seen.contains_key(&message.id) {
            return Err(invalid_import("duplicate_message_id"));
        }
        if !["user", "assistant", "tool", "system"].contains(&message.role.as_str()) {
            return Err(invalid_import("invalid_message_role"));
        }
        message.chat_id = input.chat.id.clone();
        message.run_id = None;
        message.order_key = index as u64 + 1;
        message.depth = match &message.parent_message_id {
            Some(parent) => {
                children.insert(parent.clone());
                seen.get(parent).ok_or_else(|| {
                    invalid_import(if declared.contains(parent) {
                        "forward_parent"
                    } else {
                        "unknown_parent"
                    })
                })? + 1
            }
            None => 0,
        };
        let sibling = siblings
            .entry(message.parent_message_id.clone())
            .or_default();
        message.branch_index = *sibling;
        *sibling += 1;
        seen.insert(message.id.clone(), message.depth);
    }
    let mut path: Vec<&MessageRecord> = input
        .messages
        .iter()
        .filter(|message| message.active)
        .collect();
    path.sort_by_key(|message| (message.depth, message.order_key));
    if !input.messages.is_empty() && path.is_empty() {
        return Err(invalid_import("no_active_path"));
    }
    let mut previous = None;
    for message in &path {
        if message.parent_message_id.as_ref() != previous {
            return Err(invalid_import("broken_active_chain"));
        }
        previous = Some(&message.id);
    }
    if previous.is_some_and(|leaf| children.contains(leaf)) {
        return Err(invalid_import("active_leaf_has_child"));
    }
    Ok(())
}

fn invalid_import(reason: &str) -> StoreError {
    StoreError::InvalidImport {
        reason: reason.into(),
    }
}
