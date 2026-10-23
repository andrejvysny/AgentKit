use super::*;
use agentkit_store::ChatRecord;
use serde_json::json;

impl DesktopBackend {
    pub(super) async fn chats(
        &self,
        input: &OperationRequest,
    ) -> Result<Option<OperationResponse>, Problem> {
        let store = self.host.store();
        let now = self.host.runtime().now_ms();
        let chat_id = input.params.get("chatId").map(String::as_str).unwrap_or("");
        let output = match input.operation.as_str() {
            "createChat" => output(
                201,
                projections::chat(
                    store
                        .create_chat(
                            &self.host.runtime().id(),
                            string(&input.body, "title"),
                            input.body.get("metadata").cloned().unwrap_or(json!({})),
                            now,
                        )
                        .await
                        .map_err(store_error)?,
                )?,
            ),
            "listChats" => {
                let before = input.query.get("before");
                let rows = store.list_chats(false).await.map_err(store_error)?;
                let rows = chat_page(rows, before.map(String::as_str), limit(input, 100))?;
                output(200, json!(rows))
            }
            "getChat" => {
                let record = store
                    .get_chat(chat_id)
                    .await
                    .map_err(store_error)?
                    .ok_or_else(|| missing("Chat", chat_id))?;
                let active = store
                    .list_tasks(Some(chat_id.into()))
                    .await
                    .map_err(store_error)?
                    .into_iter()
                    .filter(|t| !t.status.is_terminal())
                    .max_by_key(|t| t.enqueued_at)
                    .map(|t| t.task_id);
                let mut value = projections::chat(record)?;
                value["activeRunId"] = json!(active);
                output(200, value)
            }
            "updateChat" => output(
                200,
                projections::chat(
                    store
                        .update_chat(
                            chat_id,
                            string(&input.body, "title"),
                            input.body.get("metadata").cloned(),
                            input.body.get("archived").and_then(Value::as_bool),
                            now,
                        )
                        .await
                        .map_err(store_error)?,
                )?,
            ),
            "deleteChat" => {
                store.delete_chat(chat_id).await.map_err(store_error)?;
                output(204, Value::Null)
            }
            "listMessages" => {
                let cursor = input
                    .query
                    .get("cursor")
                    .and_then(|s| u64::from_str_radix(&s[1..], 36).ok())
                    .unwrap_or(0);
                let mut rows = store
                    .messages(chat_id, false)
                    .await
                    .map_err(store_error)?
                    .into_iter()
                    .filter(|m| m.order_key > cursor)
                    .collect::<Vec<_>>();
                let more = rows.len() > limit(input, 100);
                rows.truncate(limit(input, 100));
                let next = rows.last().map(|m| format!("m{}", base36(m.order_key)));
                let mut value = json!({"items":rows.into_iter().map(projections::message).collect::<Result<Vec<_>,_>>()?});
                if more {
                    value["nextCursor"] = json!(next);
                }
                output(200, value)
            }
            "activateBranch" => output(
                200,
                json!({"items":store.activate_path(&input.params["messageId"]).await.map_err(store_error)?.into_iter().map(projections::message).collect::<Result<Vec<_>,_>>()?}),
            ),
            "listSiblings" => {
                let id = &input.params["messageId"];
                let message = store
                    .get_message(id)
                    .await
                    .map_err(store_error)?
                    .ok_or_else(|| missing("Message", id))?;
                let mut siblings = store
                    .messages(&message.chat_id, true)
                    .await
                    .map_err(store_error)?
                    .into_iter()
                    .filter(|m| m.parent_message_id == message.parent_message_id)
                    .collect::<Vec<_>>();
                siblings.sort_by_key(|m| m.branch_index);
                output(
                    200,
                    json!(
                        siblings
                            .into_iter()
                            .map(projections::message)
                            .collect::<Result<Vec<_>, _>>()?
                    ),
                )
            }
            "forkChat" => {
                let id = &input.body["fromMessageId"].as_str().unwrap();
                let (chat, _) = store
                    .fork_chat(chat_id, id, &self.host.runtime().id(), now)
                    .await
                    .map_err(store_error)?;
                output(201, projections::chat(chat)?)
            }
            "searchMessages" => {
                let hits = store
                    .search_messages(
                        &input.query["q"],
                        input.query.get("chatId").filter(|s| !s.is_empty()).cloned(),
                        limit(input, 100),
                    )
                    .await
                    .map_err(store_error)?;
                output(
                    200,
                    json!({"hits":hits.into_iter().map(|h|json!({"chatId":h.message.chat_id,"messageId":h.message.id,"snippet":snippet(&h.message.content,&input.query["q"])})).collect::<Vec<_>>()}),
                )
            }
            _ => return Ok(None),
        };
        Ok(Some(output))
    }
}

fn chat_page(
    rows: Vec<ChatRecord>,
    before: Option<&str>,
    limit: usize,
) -> Result<Vec<Value>, Problem> {
    let mut page = Vec::new();
    for row in rows {
        let updated_at = projections::conversation_time(&row.updated_at)?;
        if before.is_some_and(|before| updated_at.as_str() >= before) {
            continue;
        }
        page.push(projections::chat(row)?);
        if page.len() == limit {
            break;
        }
    }
    Ok(page)
}
pub(super) fn string(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_owned)
}
fn base36(mut number: u64) -> String {
    let mut digits = Vec::new();
    loop {
        digits.push(b"0123456789abcdefghijklmnopqrstuvwxyz"[(number % 36) as usize] as char);
        number /= 36;
        if number == 0 {
            break;
        }
    }
    digits.into_iter().rev().collect()
}
fn snippet(value: &Value, query: &str) -> String {
    let text = if let Some(text) = value.as_str() {
        text.to_owned()
    } else {
        value
            .as_array()
            .map(|parts| {
                parts
                    .iter()
                    .filter(|p| p["type"] == "text")
                    .filter_map(|p| p["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default()
    };
    let characters: Vec<char> = text.chars().collect();
    let folded: Vec<(char, usize)> = characters
        .iter()
        .enumerate()
        .flat_map(|(index, c)| c.to_lowercase().map(move |folded| (folded, index)))
        .collect();
    let normalized: String = folded.iter().map(|(c, _)| c).collect();
    let needle = query.trim().to_lowercase();
    let Some(byte) = normalized.find(&needle) else {
        return text;
    };
    let at = normalized[..byte].chars().count();
    let length = needle.chars().count();
    if length == 0 {
        return String::new();
    }
    let first = folded[at].1;
    let last = folded[at + length - 1].1 + 1;
    let start = first.saturating_sub(40);
    let end = (last + 40).min(characters.len());
    format!(
        "{}{}[{}]{}{}",
        if start > 0 { "…" } else { "" },
        characters[start..first].iter().collect::<String>(),
        characters[first..last].iter().collect::<String>(),
        characters[last..end].iter().collect::<String>(),
        if end < characters.len() { "…" } else { "" }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn search_marks_late_matches_in_all_text_parts() {
        let content = json!([{"type":"text","text":"x".repeat(300)},{"type":"image","source":{"kind":"url","url":"https://example.test/not-a-snippet"}},{"type":"text","text":"late Zebrafish match"}]);
        let rendered = snippet(&content, "zebrafish");
        assert!(rendered.starts_with('…'));
        assert!(rendered.contains("[Zebrafish]"));
        assert!(!rendered.contains("example.test"));
        assert!(!rendered.contains("\"type\""));
    }
}
