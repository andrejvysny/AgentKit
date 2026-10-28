use crate::{
    util::{all, get, id, json, required},
    *,
};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

agentkit_contracts::impl_json_via_serde!(ChatRecord);
agentkit_contracts::impl_json_via_serde!(MessageRecord);

mod history;
mod order;
pub use history::ImportConversation;
pub use order::order_messages_for_provider;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatRecord {
    pub id: String,
    pub title: Option<String>,
    #[serde(deserialize_with = "agentkit_contracts::deserialize_preserving")]
    pub metadata: Value,
    pub archived: bool,
    pub created_at: ConversationTimestamp,
    pub updated_at: ConversationTimestamp,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageInput {
    pub id: Option<String>,
    pub chat_id: String,
    pub run_id: Option<String>,
    pub role: String,
    #[serde(deserialize_with = "agentkit_contracts::deserialize_preserving")]
    pub content: Value,
    pub parent_message_id: Option<String>,
    pub tool_call_id: Option<String>,
    #[serde(
        default,
        deserialize_with = "agentkit_contracts::deserialize_preserving"
    )]
    pub tool_calls: Option<Value>,
    pub model_result_json: Option<String>,
    #[serde(deserialize_with = "agentkit_contracts::deserialize_preserving")]
    pub metadata: Value,
    pub activate: bool,
}

impl Default for MessageInput {
    fn default() -> Self {
        Self {
            id: None,
            chat_id: String::new(),
            run_id: None,
            role: "user".into(),
            content: Value::String(String::new()),
            parent_message_id: None,
            tool_call_id: None,
            tool_calls: None,
            model_result_json: None,
            metadata: serde_json::json!({}),
            activate: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageRecord {
    pub id: String,
    pub chat_id: String,
    pub run_id: Option<String>,
    pub role: String,
    #[serde(deserialize_with = "agentkit_contracts::deserialize_preserving")]
    pub content: Value,
    pub parent_message_id: Option<String>,
    pub tool_call_id: Option<String>,
    #[serde(
        default,
        deserialize_with = "agentkit_contracts::deserialize_preserving"
    )]
    pub tool_calls: Option<Value>,
    pub model_result_json: Option<String>,
    #[serde(deserialize_with = "agentkit_contracts::deserialize_preserving")]
    pub metadata: Value,
    pub order_key: u64,
    pub depth: u32,
    pub branch_index: u32,
    pub active: bool,
    pub created_at: ConversationTimestamp,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageSearchHit {
    pub message: MessageRecord,
    pub score: usize,
}

pub(crate) fn save_message(db: &Connection, message: &MessageRecord) -> Result<()> {
    db.execute(
        "UPDATE messages SET record=? WHERE id=?",
        params![json(db, message)?, message.id],
    )?;
    Ok(())
}

pub(crate) fn project(db: &Connection, input: MessageInput, now: i64) -> Result<MessageRecord> {
    let existing = input
        .id
        .as_deref()
        .map(|id| get::<MessageRecord>(db, "messages", "id", id))
        .transpose()?
        .flatten();
    let Some(mut message) = existing else {
        return append(db, input, now);
    };
    if message.chat_id != input.chat_id
        || message.run_id != input.run_id
        || message.role != input.role
        || message.metadata.get("placeholder") != Some(&Value::Bool(true))
    {
        return Err(StoreError::InvalidInput(
            "projection may only replace its own placeholder".into(),
        ));
    }
    message.content = input.content;
    message.metadata = input.metadata;
    let metadata = message
        .metadata
        .as_object_mut()
        .ok_or_else(|| StoreError::InvalidInput("message metadata must be an object".into()))?;
    metadata.entry("placeholder").or_insert(Value::Bool(false));
    message.tool_call_id = input.tool_call_id;
    message.tool_calls = input.tool_calls;
    message.model_result_json = input.model_result_json;
    save_message(db, &message)?;
    let mut chat: ChatRecord = required(db, "chats", "id", &message.chat_id)?;
    chat.updated_at = ConversationTimestamp::from_epoch_millis(now)?;
    db.execute(
        "UPDATE chats SET record=? WHERE id=?",
        params![json(db, &chat)?, chat.id],
    )?;
    Ok(message)
}

pub(crate) fn append(db: &Connection, input: MessageInput, now: i64) -> Result<MessageRecord> {
    if !["user", "assistant", "tool", "system"].contains(&input.role.as_str()) {
        return Err(StoreError::InvalidInput("invalid message role".into()));
    }
    let mut chat: ChatRecord = required(db, "chats", "id", &input.chat_id)?;
    if !input.activate && input.parent_message_id.is_none() {
        return Err(StoreError::InvalidInput(
            "chain append requires explicit parent".into(),
        ));
    }
    let messages: Vec<MessageRecord> = all::<MessageRecord>(db, "messages")?
        .into_iter()
        .filter(|m| m.chat_id == chat.id)
        .collect();
    let parent_id = input.parent_message_id.or_else(|| {
        messages
            .iter()
            .filter(|m| m.active)
            .max_by_key(|m| m.depth)
            .map(|m| m.id.clone())
    });
    let parent = parent_id
        .as_ref()
        .map(|id| {
            messages
                .iter()
                .find(|m| m.id == *id)
                .ok_or_else(|| StoreError::NotFound(id.clone()))
        })
        .transpose()?;
    let active = input.activate
        || parent.is_some_and(|p| p.active)
            && !messages
                .iter()
                .any(|m| m.parent_message_id == parent_id && m.active);
    let message = MessageRecord {
        id: input.id.unwrap_or_else(id),
        chat_id: chat.id.clone(),
        run_id: input.run_id,
        role: input.role,
        content: input.content,
        parent_message_id: parent_id.clone(),
        tool_call_id: input.tool_call_id,
        tool_calls: input.tool_calls,
        model_result_json: input.model_result_json,
        metadata: input.metadata,
        order_key: messages.iter().map(|m| m.order_key).max().unwrap_or(0) + 1,
        depth: parent.map_or(0, |p| p.depth + 1),
        branch_index: messages
            .iter()
            .filter(|m| m.parent_message_id == parent_id)
            .map(|m| m.branch_index + 1)
            .max()
            .unwrap_or(0),
        active,
        created_at: ConversationTimestamp::from_epoch_millis(now)?,
    };
    if get::<MessageRecord>(db, "messages", "id", &message.id)?.is_some() {
        return Err(StoreError::Duplicate(message.id));
    }
    db.execute(
        "INSERT INTO messages VALUES(?,?,?,?,?)",
        params![
            message.id,
            message.chat_id,
            message.parent_message_id,
            crate::util::sql_integer(message.order_key)?,
            json(db, &message)?
        ],
    )?;
    if input.activate {
        activate(db, &message.id)?;
    }
    chat.updated_at = ConversationTimestamp::from_epoch_millis(now)?;
    db.execute(
        "UPDATE chats SET record=? WHERE id=?",
        params![json(db, &chat)?, chat.id],
    )?;
    Ok(message)
}

pub(crate) fn activate(db: &Connection, message_id: &str) -> Result<Vec<MessageRecord>> {
    let target: MessageRecord = required(db, "messages", "id", message_id)?;
    let mut messages: Vec<MessageRecord> = all::<MessageRecord>(db, "messages")?
        .into_iter()
        .filter(|m| m.chat_id == target.chat_id)
        .collect();
    let mut leaf = target;
    while let Some(child) = messages
        .iter()
        .filter(|m| m.parent_message_id.as_deref() == Some(&leaf.id))
        .min_by_key(|m| m.branch_index)
    {
        leaf = child.clone();
    }
    let mut path = Vec::new();
    let mut cursor = Some(leaf.id);
    while let Some(id) = cursor {
        let node = messages
            .iter()
            .find(|m| m.id == id)
            .ok_or_else(|| StoreError::NotFound(id.clone()))?;
        path.push(id);
        cursor = node.parent_message_id.clone();
    }
    for message in &mut messages {
        message.active = path.contains(&message.id);
        save_message(db, message)?;
    }
    messages.retain(|m| m.active);
    messages.sort_by_key(|m| m.order_key);
    Ok(messages)
}

impl SqliteStore {
    pub async fn create_chat(
        &self,
        chat_id: &str,
        title: Option<String>,
        metadata: Value,
        now: i64,
    ) -> Result<ChatRecord> {
        let chat_id = chat_id.to_owned();
        self.call(move |db| {
            if get::<ChatRecord>(db, "chats", "id", &chat_id)?.is_some() {
                return Err(StoreError::Duplicate(chat_id));
            }
            let chat = ChatRecord {
                id: chat_id,
                title,
                metadata,
                archived: false,
                created_at: ConversationTimestamp::from_epoch_millis(now)?,
                updated_at: ConversationTimestamp::from_epoch_millis(now)?,
            };
            db.execute(
                "INSERT INTO chats VALUES(?,?)",
                params![chat.id, json(db, &chat)?],
            )?;
            Ok(chat)
        })
        .await
    }

    pub async fn get_chat(&self, chat_id: &str) -> Result<Option<ChatRecord>> {
        let chat_id = chat_id.to_owned();
        self.call(move |db| get(db, "chats", "id", &chat_id)).await
    }

    pub async fn list_chats(&self, include_archived: bool) -> Result<Vec<ChatRecord>> {
        self.call(move |db| {
            let chats: Vec<ChatRecord> = all::<ChatRecord>(db, "chats")?
                .into_iter()
                .filter(|c| include_archived || !c.archived)
                .collect();
            sort_chats(chats)
        })
        .await
    }

    pub async fn update_chat(
        &self,
        chat_id: &str,
        title: Option<String>,
        metadata: Option<Value>,
        archived: Option<bool>,
        now: i64,
    ) -> Result<ChatRecord> {
        let chat_id = chat_id.to_owned();
        self.call(move |db| {
            let mut chat: ChatRecord = required(db, "chats", "id", &chat_id)?;
            if title.is_some() {
                chat.title = title;
            }
            if let Some(value) = metadata {
                chat.metadata = value;
            }
            if let Some(value) = archived {
                chat.archived = value;
            }
            chat.updated_at = ConversationTimestamp::from_epoch_millis(now)?;
            db.execute(
                "UPDATE chats SET record=? WHERE id=?",
                params![json(db, &chat)?, chat_id],
            )?;
            Ok(chat)
        })
        .await
    }

    /// Read-modify-write the chat record inside ONE actor job, so a
    /// concurrent writer cannot slip between the read and the write and
    /// silently drop this update. A closure error rolls the write back.
    pub async fn mutate_chat<F, T>(&self, chat_id: &str, now: i64, mutate: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut ChatRecord) -> Result<T> + Send + 'static,
    {
        let chat_id = chat_id.to_owned();
        self.call(move |db| {
            let mut chat: ChatRecord = required(db, "chats", "id", &chat_id)?;
            let value = mutate(&mut chat)?;
            chat.updated_at = ConversationTimestamp::from_epoch_millis(now)?;
            db.execute(
                "UPDATE chats SET record=? WHERE id=?",
                params![json(db, &chat)?, chat_id],
            )?;
            Ok(value)
        })
        .await
    }

    pub async fn append_message(
        &self,
        input: MessageInput,
        proof: Option<LeaseFence>,
        now: i64,
    ) -> Result<MessageRecord> {
        self.call(move |db| {
            let transaction = db.transaction()?;
            if let Some(proof) = proof {
                crate::tasks::fence(&transaction, &proof, now)?;
                if input.run_id.as_deref() != Some(&proof.task_id) {
                    return Err(StoreError::InvalidInput(
                        "message run does not match lease task".into(),
                    ));
                }
            }
            let message = append(&transaction, input, now)?;
            transaction.commit()?;
            Ok(message)
        })
        .await
    }

    pub async fn get_message(&self, message_id: &str) -> Result<Option<MessageRecord>> {
        let message_id = message_id.to_owned();
        self.call(move |db| get(db, "messages", "id", &message_id))
            .await
    }

    pub async fn messages(
        &self,
        chat_id: &str,
        include_inactive: bool,
    ) -> Result<Vec<MessageRecord>> {
        let chat_id = chat_id.to_owned();
        self.call(move |db| {
            let _: ChatRecord = required(db, "chats", "id", &chat_id)?;
            let mut messages: Vec<MessageRecord> = all::<MessageRecord>(db, "messages")?
                .into_iter()
                .filter(|m| m.chat_id == chat_id && (include_inactive || m.active))
                .collect();
            messages.sort_by_key(|m| m.order_key);
            Ok(messages)
        })
        .await
    }

    pub async fn activate_path(&self, message_id: &str) -> Result<Vec<MessageRecord>> {
        let message_id = message_id.to_owned();
        self.call(move |db| {
            let transaction = db.transaction()?;
            let path = activate(&transaction, &message_id)?;
            transaction.commit()?;
            Ok(path)
        })
        .await
    }

    pub async fn search_messages(
        &self,
        query: &str,
        chat_id: Option<String>,
        limit: usize,
    ) -> Result<Vec<MessageSearchHit>> {
        let query = query.trim().to_lowercase();
        self.call(move |db| {
            if query.is_empty() || !query.chars().any(char::is_alphanumeric) {
                return Ok(Vec::new());
            }
            let mut hits = Vec::new();
            for message in all::<MessageRecord>(db, "messages")? {
                if chat_id.as_ref().is_some_and(|id| *id != message.chat_id)
                    || message.metadata.get("internal") == Some(&Value::Bool(true))
                    || message.metadata.get("placeholder") == Some(&Value::Bool(true))
                {
                    continue;
                }
                let score = search_text(&message.content)
                    .to_lowercase()
                    .matches(&query)
                    .count();
                if score > 0 {
                    hits.push(MessageSearchHit { message, score });
                }
            }
            hits.sort_by_key(|hit| std::cmp::Reverse(hit.score));
            hits.truncate(limit);
            Ok(hits)
        })
        .await
    }
}

fn sort_chats(chats: Vec<ChatRecord>) -> Result<Vec<ChatRecord>> {
    let mut keyed = chats
        .into_iter()
        .map(|chat| Ok((chat.updated_at.wire_text()?.into_owned(), chat)))
        .collect::<Result<Vec<_>>>()?;
    keyed.sort_by(|(left, _), (right, _)| right.cmp(left));
    Ok(keyed.into_iter().map(|(_, chat)| chat).collect())
}

fn search_text(content: &Value) -> String {
    if let Some(text) = content.as_str() {
        return text.into();
    }
    content
        .as_array()
        .map(|parts| {
            parts
                .iter()
                .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}
