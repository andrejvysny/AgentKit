//! The `conversations.*` port surface: the `ConversationStore` half of the
//! assistant store, dispatched through `ports::execute` so it runs identically
//! standalone and inside an open transaction view. Every arm is thin — parse
//! the port's parameters, call exactly one engine operation, serialize the
//! store's native record JSON. Ordering, tree invariants, and atomicity stay
//! in `agentkit-store`.

use super::ports::{
    deserialized, flag, instant_or_now, nonnegative_i64, opt_instant, opt_text, optional_fence,
    optional_record, record, records, required_i64, text,
};
use crate::errors;
use agentkit_store::{
    ChatRecord, ConversationTimestamp, ImportConversation, MessageInput, SqliteStore,
};
use napi::bindgen_prelude::*;
use serde_json::{Value, json};

const DEFAULT_SEARCH_LIMIT: usize = 20;

pub(super) async fn execute(store: &SqliteStore, method: &str, input: Value) -> Result<Value> {
    match method {
        "createChat" => {
            let source = &input["input"];
            let chat_id = match opt_text(source, "id")? {
                Some(id) => id.to_owned(),
                None => agentkit_store::id(),
            };
            let title = opt_text(source, "title")?.map(str::to_owned);
            let metadata = match source.get("metadata") {
                None | Some(Value::Null) => json!({}),
                Some(value) => value.clone(),
            };
            let now = instant_or_now(&input, "now")?;
            Ok(record(
                store
                    .create_chat(&chat_id, title, metadata, now)
                    .await
                    .map_err(errors::store)?,
            ))
        }
        "getChat" => Ok(optional_record(
            store
                .get_chat(text(&input, "chatId")?)
                .await
                .map_err(errors::store)?,
        )),
        "listChats" => {
            let opts = input.get("opts").cloned().unwrap_or(Value::Null);
            let include_archived = flag(&opts, "includeArchived", false)?;
            let ids = match opts.get("ids") {
                None | Some(Value::Null) => None,
                Some(_) => Some(super::ports::string_array(&opts, "ids")?),
            };
            let before = opt_instant(&opts, "before")?;
            let limit = match opts.get("limit") {
                None | Some(Value::Null) => None,
                Some(_) => Some(nonnegative_i64(&opts, "limit")? as usize),
            };
            // `ids` overrides the archived default: naming a chat is a claim
            // that this chat specifically is wanted.
            let mut chats = store
                .list_chats(include_archived || ids.is_some())
                .await
                .map_err(errors::store)?;
            if let Some(ids) = ids {
                if ids.is_empty() {
                    return Ok(Value::Array(Vec::new()));
                }
                chats.retain(|chat| ids.contains(&chat.id));
            }
            if let Some(before) = before {
                chats.retain(|chat| chat_updated_ms(chat).map(|at| at < before).unwrap_or(true));
            }
            if let Some(limit) = limit {
                chats.truncate(limit);
            }
            Ok(records(chats))
        }
        "updateChat" => {
            let patch = input.get("patch").cloned().unwrap_or(Value::Null);
            Ok(record(
                store
                    .update_chat(
                        text(&input, "chatId")?,
                        opt_text(&patch, "title")?.map(str::to_owned),
                        match patch.get("metadata") {
                            None | Some(Value::Null) => None,
                            Some(value) => Some(value.clone()),
                        },
                        match patch.get("archived") {
                            None | Some(Value::Null) => None,
                            Some(Value::Bool(value)) => Some(*value),
                            _ => {
                                return Err(errors::invalid("patch.archived must be a boolean"));
                            }
                        },
                        instant_or_now(&input, "now")?,
                    )
                    .await
                    .map_err(errors::store)?,
            ))
        }
        "deleteChat" => {
            store
                .delete_chat(text(&input, "chatId")?)
                .await
                .map_err(errors::store)?;
            Ok(Value::Null)
        }
        "appendMessage" => {
            let mut message: MessageInput = deserialized(&input, "input")?;
            // JSON erases the port's absent-vs-null parent: an explicit null
            // means "another root", which the engine reads from `explicitRoot`.
            message.explicit_root =
                matches!(input["input"].get("parentMessageId"), Some(Value::Null));
            let proof = optional_fence(store, &input, "fence").await?;
            Ok(record(
                store
                    .append_message(message, proof, instant_or_now(&input, "now")?)
                    .await
                    .map_err(errors::store)?,
            ))
        }
        "updateMessage" => {
            let patch = input.get("patch").cloned().unwrap_or(Value::Null);
            let proof = optional_fence(store, &input, "fence").await?;
            let content = match patch.get("content") {
                None | Some(Value::Null) => None,
                Some(value) => Some(value.clone()),
            };
            let metadata = match patch.get("metadata") {
                None | Some(Value::Null) => None,
                Some(value) => Some(value.clone()),
            };
            let tool_calls = match patch.get("toolCalls") {
                None | Some(Value::Null) => None,
                Some(value) => Some(value.clone()),
            };
            Ok(record(
                store
                    .update_message(
                        text(&input, "messageId")?,
                        content,
                        metadata,
                        tool_calls,
                        proof,
                        instant_or_now(&input, "now")?,
                    )
                    .await
                    .map_err(errors::store)?,
            ))
        }
        "listMessages" => {
            let opts = input.get("opts").cloned().unwrap_or(Value::Null);
            let after = opt_instant(&opts, "afterOrderKey")?;
            let before = opt_instant(&opts, "beforeOrderKey")?;
            if after.is_some() && before.is_some() {
                return Err(errors::invalid("invalid_cursor"));
            }
            let limit = match opts.get("limit") {
                None | Some(Value::Null) => None,
                Some(_) => Some(nonnegative_i64(&opts, "limit")? as usize),
            };
            let messages = store
                .messages(text(&input, "chatId")?, false)
                .await
                .map_err(errors::store)?;
            // The engine returns the path in ascending order; the port's
            // `limit` counts from the recent end of the selected window.
            let mut selected = messages
                .into_iter()
                .filter(|message| {
                    after.is_none_or(|cursor| message.order_key as i64 > cursor)
                        && before.is_none_or(|cursor| (message.order_key as i64) < cursor)
                })
                .collect::<Vec<_>>();
            if let Some(limit) = limit {
                let skip = selected.len().saturating_sub(limit);
                selected = selected.split_off(skip);
            }
            Ok(records(selected))
        }
        "getMessage" => Ok(optional_record(
            store
                .get_message(text(&input, "messageId")?)
                .await
                .map_err(errors::store)?,
        )),
        "listTree" => {
            let options = input.get("options").cloned().unwrap_or(Value::Null);
            let include_archived = flag(&options, "includeArchived", false)?;
            let messages = store
                .messages(text(&input, "chatId")?, true)
                .await
                .map_err(errors::store)?;
            Ok(records(
                messages
                    .into_iter()
                    .filter(|message| include_archived || !message.archived)
                    .collect(),
            ))
        }
        "archiveBranch" => Ok(json!({
            "archivedCount": store
                .archive_branch(text(&input, "messageId")?)
                .await
                .map_err(errors::store)?
        })),
        "lastMessageOfRun" => Ok(optional_record(
            store
                .last_message_of_run(text(&input, "chatId")?, text(&input, "runId")?)
                .await
                .map_err(errors::store)?,
        )),
        "listSiblings" => Ok(records(
            store
                .list_siblings(text(&input, "messageId")?)
                .await
                .map_err(errors::store)?,
        )),
        "activatePath" => Ok(records(
            store
                .activate_path(text(&input, "messageId")?)
                .await
                .map_err(errors::store)?,
        )),
        "forkChat" => {
            let new_chat_id = match opt_text(&input, "newChatId")? {
                Some(id) => id.to_owned(),
                None => agentkit_store::id(),
            };
            let (chat, messages) = store
                .fork_chat(
                    text(&input, "chatId")?,
                    text(&input, "fromMessageId")?,
                    &new_chat_id,
                    instant_or_now(&input, "now")?,
                )
                .await
                .map_err(errors::store)?;
            Ok(json!({"chat": record(chat), "messages": records(messages)}))
        }
        "importConversation" => {
            let source = &input["input"];
            let chat_source = &source["chat"];
            let now = instant_or_now(&input, "now")?;
            let created = match chat_source.get("createdAt") {
                None | Some(Value::Null) => ConversationTimestamp::from_epoch_millis(now)
                    .map_err(|error| errors::store(error.into()))?,
                Some(Value::String(value)) => ConversationTimestamp::from(value.clone()),
                Some(_) => ConversationTimestamp::from_epoch_millis(required_i64(
                    chat_source,
                    "createdAt",
                )?)
                .map_err(|error| errors::store(error.into()))?,
            };
            let chat = ChatRecord {
                id: text(chat_source, "id")?.to_owned(),
                title: opt_text(chat_source, "title")?.map(str::to_owned),
                metadata: match chat_source.get("metadata") {
                    None | Some(Value::Null) => json!({}),
                    Some(value) => value.clone(),
                },
                archived: flag(chat_source, "archived", false)?,
                created_at: created.clone(),
                updated_at: created.clone(),
            };
            let raw = source["messages"]
                .as_array()
                .ok_or_else(|| errors::invalid("input.messages must be an array"))?
                .clone();
            let mut messages = Vec::with_capacity(raw.len());
            for item in raw {
                let mut object = item
                    .as_object()
                    .ok_or_else(|| errors::invalid("imported messages must be objects"))?
                    .clone();
                // Identity fields the import owns; derived fields the engine recomputes.
                object.insert("chatId".into(), json!(""));
                object.insert("runId".into(), Value::Null);
                object.insert("orderKey".into(), json!(0));
                object.insert("depth".into(), json!(0));
                object.insert("branchIndex".into(), json!(0));
                if object.get("internal") == Some(&Value::Bool(true)) {
                    object.remove("internal");
                    let metadata = object
                        .entry("metadata".to_owned())
                        .or_insert_with(|| json!({}));
                    let Some(map) = metadata.as_object_mut() else {
                        return Err(errors::invalid("metadata must be an object"));
                    };
                    map.insert("internal".into(), Value::Bool(true));
                }
                match object.get("metadata") {
                    None => {
                        object.insert("metadata".into(), json!({}));
                    }
                    Some(Value::Null) => {
                        object.insert("metadata".into(), json!({}));
                    }
                    Some(Value::Object(_)) => {}
                    _ => return Err(errors::invalid("metadata must be an object")),
                }
                match object.get("createdAt") {
                    None | Some(Value::Null) => {
                        object.insert("createdAt".into(), record(created.clone()));
                    }
                    Some(Value::String(_) | Value::Number(_)) => {}
                    _ => return Err(errors::invalid("createdAt must be an instant")),
                }
                messages.push(
                    serde_json::from_value(Value::Object(object)).map_err(|error| {
                        errors::invalid(format!("invalid imported message: {error}"))
                    })?,
                );
            }
            let chat_record = record(
                store
                    .import_conversation(ImportConversation { chat, messages })
                    .await
                    .map_err(errors::store)?,
            );
            Ok(chat_record)
        }
        "searchMessages" => {
            let opts = input.get("opts").cloned().unwrap_or(Value::Null);
            let chat_id = opt_text(&opts, "chatId")?.map(str::to_owned);
            let limit = match opts.get("limit") {
                None | Some(Value::Null) => DEFAULT_SEARCH_LIMIT,
                Some(_) => nonnegative_i64(&opts, "limit")? as usize,
            };
            Ok(records(
                store
                    .search_messages(text(&input, "query")?, chat_id, limit)
                    .await
                    .map_err(errors::store)?,
            ))
        }
        _ => Err(errors::stable(
            "not_implemented",
            "Unsupported conversation store operation",
            None,
        )),
    }
}

fn chat_updated_ms(chat: &ChatRecord) -> Option<i64> {
    chat.updated_at.epoch_millis().or_else(|| {
        chat.updated_at
            .wire_text()
            .ok()
            .and_then(|text| agentkit_store::parse_answer_expiry(&text))
    })
}

#[cfg(test)]
mod tests {
    use crate::store::ports::testkit::{NOW, begin, call, code, store};
    use serde_json::json;

    #[tokio::test]
    async fn chat_and_message_round_trip_through_the_port_surface() {
        let store = store();
        let chat = call(
            &store,
            "conversations.createChat",
            json!({"input":{"id":"c1","title":"Trip","metadata":{"tag":"x"}},"now":NOW}),
        )
        .await
        .expect("chat created");
        assert_eq!(chat["id"], "c1");
        assert_eq!(chat["title"], "Trip");
        assert_eq!(chat["metadata"]["tag"], "x");
        assert_eq!(
            call(&store, "conversations.getChat", json!({"chatId":"c1"}))
                .await
                .unwrap()["id"],
            "c1"
        );
        assert!(
            call(&store, "conversations.getChat", json!({"chatId":"ghost"}))
                .await
                .unwrap()
                .is_null(),
            "an absent chat renders as null, never an error"
        );

        let message = call(
            &store,
            "conversations.appendMessage",
            json!({"input":{"chatId":"c1","role":"user","content":"hello world","parentMessageId":null},"now":NOW}),
        )
        .await
        .expect("root message appended");
        assert_eq!(message["role"], "user");
        let message_id = message["id"].as_str().expect("generated id").to_owned();
        assert_eq!(
            call(&store, "conversations.listMessages", json!({"chatId":"c1"}))
                .await
                .unwrap()
                .as_array()
                .expect("list")
                .len(),
            1
        );
        assert_eq!(
            call(
                &store,
                "conversations.getMessage",
                json!({"messageId":message_id})
            )
            .await
            .unwrap()["id"],
            message_id
        );
        assert_eq!(
            call(&store, "conversations.listTree", json!({"chatId":"c1"}))
                .await
                .unwrap()
                .as_array()
                .expect("tree")
                .len(),
            1
        );
        assert_eq!(
            call(
                &store,
                "conversations.searchMessages",
                json!({"query":"hello","opts":{}})
            )
            .await
            .unwrap()
            .as_array()
            .expect("hits")
            .len(),
            1
        );

        let chats = call(&store, "conversations.listChats", json!({"opts":{}}))
            .await
            .unwrap();
        assert_eq!(chats.as_array().expect("list").len(), 1);
        call(
            &store,
            "conversations.updateChat",
            json!({"chatId":"c1","patch":{"archived":true},"now":NOW}),
        )
        .await
        .expect("archived");
        assert!(
            call(&store, "conversations.listChats", json!({"opts":{}}))
                .await
                .unwrap()
                .as_array()
                .expect("list")
                .is_empty(),
            "the default listing hides archived chats"
        );
        assert_eq!(
            call(
                &store,
                "conversations.listChats",
                json!({"opts":{"includeArchived":true}})
            )
            .await
            .unwrap()
            .as_array()
            .expect("list")
            .len(),
            1,
            "ids-archived chats return when asked for"
        );
    }

    #[tokio::test]
    async fn malformed_input_and_missing_parents_are_typed_errors() {
        let store = store();
        assert_eq!(
            code(
                call(
                    &store,
                    "conversations.forkChat",
                    json!({"chatId":"ghost","fromMessageId":"m1","now":NOW})
                )
                .await
                .unwrap_err()
            ),
            "not_found",
            "forking a chat that never existed is a store not_found"
        );
        assert_eq!(
            code(
                call(
                    &store,
                    "conversations.appendMessage",
                    json!({"input":{"role":"user","content":"x"}})
                )
                .await
                .unwrap_err()
            ),
            "invalid_input",
            "a message without its chat is malformed input"
        );
        assert_eq!(
            code(
                call(
                    &store,
                    "conversations.listChats",
                    json!({"opts":{"limit":-1}})
                )
                .await
                .unwrap_err()
            ),
            "invalid_input"
        );
        assert_eq!(
            code(
                call(
                    &store,
                    "conversations.listMessages",
                    json!({"chatId":"c1","opts":{"afterOrderKey":5,"beforeOrderKey":9}})
                )
                .await
                .unwrap_err()
            ),
            "invalid_input",
            "the two cursors are mutually exclusive"
        );
        assert_eq!(
            code(
                call(&store, "conversations.teleport", json!({}))
                    .await
                    .unwrap_err()
            ),
            "not_implemented",
            "unknown operations refuse before any effect"
        );
    }

    #[tokio::test]
    async fn committed_conversation_writes_become_visible() {
        let store = store();
        let transaction = begin(&store).await;
        let view = transaction.view();
        call(
            &view,
            "conversations.createChat",
            json!({"input":{"id":"c9"},"now":NOW}),
        )
        .await
        .expect("chat inside the transaction");
        call(
            &view,
            "conversations.appendMessage",
            json!({"input":{"chatId":"c9","role":"user","content":"inside","parentMessageId":null},"now":NOW}),
        )
        .await
        .expect("message inside the transaction");
        transaction.commit().await.expect("commit");
        assert_eq!(
            call(&store, "conversations.getChat", json!({"chatId":"c9"}))
                .await
                .unwrap()["id"],
            "c9"
        );
        assert_eq!(
            call(&store, "conversations.listMessages", json!({"chatId":"c9"}))
                .await
                .unwrap()
                .as_array()
                .expect("list")
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn rolled_back_conversation_writes_leave_no_trace() {
        let store = store();
        let transaction = begin(&store).await;
        let view = transaction.view();
        call(
            &view,
            "conversations.createChat",
            json!({"input":{"id":"c9"},"now":NOW}),
        )
        .await
        .expect("chat inside the transaction");
        call(
            &view,
            "conversations.appendMessage",
            json!({"input":{"chatId":"c9","role":"user","content":"doomed","parentMessageId":null},"now":NOW}),
        )
        .await
        .expect("message inside the transaction");
        transaction.rollback().await.expect("rollback");
        assert!(
            call(&store, "conversations.getChat", json!({"chatId":"c9"}))
                .await
                .unwrap()
                .is_null()
        );
        assert!(
            call(
                &store,
                "conversations.listChats",
                json!({"opts":{"includeArchived":true}})
            )
            .await
            .unwrap()
            .as_array()
            .expect("list")
            .is_empty(),
            "nothing persisted, not even an archived remnant"
        );
    }
}
