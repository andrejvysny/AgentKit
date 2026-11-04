use crate::{
    util::{all, get, id, json, required, select},
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
    #[serde(
        default = "empty_metadata",
        deserialize_with = "agentkit_contracts::deserialize_preserving"
    )]
    pub metadata: Value,
    #[serde(default = "activate_default")]
    pub activate: bool,
    /// The TypeScript port distinguishes an absent `parentMessageId` (append to
    /// the active leaf) from an explicit `null` (create another root). JSON
    /// erases that difference, so the binding sets this flag for the explicit
    /// root case and `append` then ignores the active leaf.
    #[serde(default)]
    pub explicit_root: bool,
}

fn empty_metadata() -> Value {
    Value::Object(Default::default())
}

fn activate_default() -> bool {
    true
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
            explicit_root: false,
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
    /// Archived messages remain readable by id for audit but never replay:
    /// `archive_branch` marks a whole branch and the listing ports hide them.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub archived: bool,
    pub created_at: ConversationTimestamp,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageSearchHit {
    pub message: MessageRecord,
    /// bm25-derived relevance, HIGHER is better (SQLite's bm25 is a
    /// "lower is better" rank, mirrored around zero; see `relevance_score`).
    /// Hits arrive in rank order already — the order,
    /// not this number, is the ranking contract.
    pub score: usize,
}

/// Update a message row. The persistent FTS5 `message_search` index is
/// kept in step by the schema's UPDATE trigger — no write path maintains
/// it by hand, which is what lets the TypeScript import's direct
/// INSERT ... ON CONFLICT and the chat-deletion cascade stay indexed too.
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

/// One chat's messages in insertion order, filtered in SQL (`messages_chat`).
pub(crate) fn chat_messages(db: &Connection, chat_id: &str) -> Result<Vec<MessageRecord>> {
    select(
        db,
        "SELECT record FROM messages WHERE chat_id=? ORDER BY rowid",
        [chat_id],
    )
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
    let messages = chat_messages(db, &chat.id)?;
    let parent_id = if input.explicit_root {
        None
    } else {
        input.parent_message_id.or_else(|| {
            messages
                .iter()
                .filter(|m| m.active)
                .max_by_key(|m| m.depth)
                .map(|m| m.id.clone())
        })
    };
    let parent = parent_id
        .as_ref()
        .map(|id| {
            messages
                .iter()
                .find(|m| m.id == *id)
                // An archived parent does not exist for appends: reviving a
                // retired branch through a new child is refused like a
                // missing one.
                .filter(|m| !m.archived)
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
        archived: false,
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
    if target.archived {
        // An archived branch is not revivable: to the tree it does not exist.
        return Err(StoreError::NotFound(message_id.to_owned()));
    }
    let mut messages = chat_messages(db, &target.chat_id)?;
    let mut leaf = target;
    while let Some(child) = {
        let kids: Vec<&MessageRecord> = messages
            .iter()
            .filter(|m| m.parent_message_id.as_deref() == Some(&leaf.id) && !m.archived)
            .collect();
        kids.iter()
            .find(|m| m.active)
            .or_else(|| kids.iter().min_by_key(|m| m.branch_index))
            .copied()
            .cloned()
    } {
        leaf = child;
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
        let active = path.contains(&message.id);
        if message.active != active {
            message.active = active;
            save_message(db, message)?;
        }
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
            let transaction = crate::transaction::atomic(db)?;
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

    /// The newest message a run wrote in this chat, or `None` when the run
    /// never persisted one. Answers the placeholder seed for a turn.
    pub async fn last_message_of_run(
        &self,
        chat_id: &str,
        run_id: &str,
    ) -> Result<Option<MessageRecord>> {
        let (chat_id, run_id) = (chat_id.to_owned(), run_id.to_owned());
        self.call(move |db| crate::session_ops::last_message_of_run(db, &chat_id, &run_id))
            .await
    }

    pub async fn messages(
        &self,
        chat_id: &str,
        include_inactive: bool,
    ) -> Result<Vec<MessageRecord>> {
        let chat_id = chat_id.to_owned();
        self.call(move |db| {
            // A chat that is gone (or never was) lists no messages; the
            // reference adapters answer with the empty listing, and callers
            // legitimately read trees of chats that were just deleted.
            let mut messages = chat_messages(db, &chat_id)?;
            messages.retain(|m| include_inactive || m.active);
            messages.sort_by_key(|m| m.order_key);
            Ok(messages)
        })
        .await
    }

    pub async fn activate_path(&self, message_id: &str) -> Result<Vec<MessageRecord>> {
        let message_id = message_id.to_owned();
        self.call(move |db| {
            let transaction = crate::transaction::atomic(db)?;
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
        // Early exits with zero database round-trips: an empty or
        // whitespace-only query (or one that sanitizes to nothing, `"***"`)
        // has no hit to find and must NOT degrade into a corpus scan, and a
        // zero limit can never return a hit. Neither even pays an actor job.
        let Some(match_query) = to_fts_query(query) else {
            return Ok(Vec::new());
        };
        if limit == 0 {
            return Ok(Vec::new());
        }
        self.call(move |db| {
            // The predicate IS the persistent FTS5 index: MATCH, the
            // `unicode61 remove_diacritics 2` tokenizer and bm25 ranking
            // are SQLite's own, exactly as the TypeScript adapters answered
            // with `message_search MATCH`. A hand-rolled predicate drifts
            // from the engine — LIKE-style substrings hit inside words the
            // tokenizer never produces, and a raw-token re-count scores a
            // diacritic-folded match at zero — so Rust only chooses the
            // query (`to_fts_query`), never evaluates one.
            //
            // The chat/visibility constraints run HERE, at query time, as a
            // filter over the join back to `messages` — where the adapter
            // keeps them, because `placeholder` and `internal` are metadata
            // keys rewritten AFTER indexing (a placeholder becomes a real
            // answer the moment its run finishes); an index that had decided
            // membership at insert time would keep every finished answer
            // permanently unfindable. Inactive branches stay searchable —
            // the tree ports' own contract — while archived, internal and
            // placeholder records never surface. `json_type(...) IS NOT
            // 'true'` is the exact counterpart of the old predicate's
            // `!= Some(Value::Bool(true))`: only a JSON `true` hides.
            //
            // Ordering is (rank, rank_index, id): the bm25 rank, then the
            // message's position in its chat, then its id — a TOTAL order
            // (`order_key` is unique per chat, `id` globally), so equal-rank
            // pages are stable across repeated calls and LIMIT pagination
            // can neither skip nor repeat a hit.
            let mut statement = db.prepare(
                "SELECT messages.record, bm25(message_search) \
                 FROM message_search \
                 JOIN messages ON messages.rowid = message_search.rowid \
                 WHERE message_search MATCH ?1 \
                   AND (?2 IS NULL OR messages.chat_id = ?2) \
                   AND json_type(messages.record, '$.archived') IS NOT 'true' \
                   AND json_type(messages.record, '$.metadata.internal') IS NOT 'true' \
                   AND json_type(messages.record, '$.metadata.placeholder') IS NOT 'true' \
                 ORDER BY bm25(message_search), messages.order_key, messages.id \
                 LIMIT ?3",
            )?;
            let rows = statement
                .query_map(params![match_query, chat_id, limit as i64], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(statement);
            rows.into_iter()
                .map(|(record, rank)| {
                    Ok(MessageSearchHit {
                        message: serde_json::from_str(&record)?,
                        score: relevance_score(rank),
                    })
                })
                .collect()
        })
        .await
    }
}

/// The hit score, derived from SQLite's own bm25 rank rather than a re-count
/// of the query's raw tokens: bm25 is a "lower is better" REAL mirrored here
/// around zero at a fixed scale so a better hit scores higher. The ORDER BY
/// in [`SqliteStore::search_messages`] is the ranking contract; this number
/// is what bm25 said about the hit (the old substring re-count scored
/// "resume" over "résumé" at zero and counted "(2)" as a token the index
/// never held).
fn relevance_score(rank: f64) -> usize {
    (-rank * 1_000_000.0).round().clamp(0.0, usize::MAX as f64) as usize
}

/// The TypeScript `toFtsQuery`, verbatim in behaviour: FTS5's operator
/// characters never reach the engine from a search box. Quotes, `*` and `^`
/// are stripped (quotes FIRST, so nothing can break out of the quoting this
/// adds), parentheses become spaces, and every whitespace token is re-emitted
/// as a quoted phrase — juxtaposed phrases are FTS5's implicit AND, which is
/// what a person typing several words means. `None` when nothing survives;
/// callers answer with no hits.
fn to_fts_query(query: &str) -> Option<String> {
    let sanitized = query.replace(['"', '*', '^'], "").replace(['(', ')'], " ");
    let tokens = sanitized
        .split_whitespace()
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>();
    if tokens.is_empty() {
        return None;
    }
    Some(
        tokens
            .iter()
            .map(|token| format!("\"{token}\""))
            .collect::<Vec<_>>()
            .join(" "),
    )
}

fn sort_chats(chats: Vec<ChatRecord>) -> Result<Vec<ChatRecord>> {
    let mut keyed = chats
        .into_iter()
        .map(|chat| Ok((chat.updated_at.wire_text()?.into_owned(), chat)))
        .collect::<Result<Vec<_>>>()?;
    keyed.sort_by(|(left, _), (right, _)| right.cmp(left));
    Ok(keyed.into_iter().map(|(_, chat)| chat).collect())
}
