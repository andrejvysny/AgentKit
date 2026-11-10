//! Session management methods exposed without duplicating host policy.
use super::{SessionService, parse_object, session_state_json};
use crate::errors;
use agentkit_engine::{Runtime, SystemRuntime};
use agentkit_store::SqliteStore;
use napi::bindgen_prelude::*;
use serde_json::Value;

#[napi]
impl SessionService {
    #[napi]
    pub async fn set_execution_mode(&self, session_id: String, mode: String) -> Result<String> {
        let guard = self.core.lock().await;
        let core = guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        Ok(core
            .runtime
            .service()
            .set_execution_mode(&session_id, &mode)
            .await
            .map_err(errors::session)?
            .to_string())
    }

    #[napi]
    pub async fn list_approval_grants(&self, session_id: String) -> Result<String> {
        let guard = self.core.lock().await;
        let core = guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        Ok(Value::Array(
            core.runtime
                .service()
                .list_approval_grants(&session_id)
                .await
                .map_err(errors::session)?,
        )
        .to_string())
    }

    #[napi]
    pub async fn revoke_approval_grant(
        &self,
        session_id: String,
        grant_id: String,
    ) -> Result<String> {
        let guard = self.core.lock().await;
        let core = guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        Ok(core
            .runtime
            .service()
            .revoke_approval_grant(&session_id, &grant_id)
            .await
            .map_err(errors::session)?
            .to_string())
    }

    #[napi]
    pub async fn compact(&self, session_id: String) -> Result<()> {
        let guard = self.core.lock().await;
        let core = guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        core.runtime
            .service()
            .compact(&session_id)
            .await
            .map_err(errors::session)
    }

    #[napi]
    pub async fn fork(&self, session_id: String, from_message_id: String) -> Result<String> {
        let guard = self.core.lock().await;
        let core = guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        let state = core
            .runtime
            .service()
            .fork(&session_id, &from_message_id)
            .await
            .map_err(errors::session)?;
        Ok(session_state_json(&state))
    }

    #[napi]
    pub async fn set_model(&self, session_id: String, input_json: String) -> Result<String> {
        let guard = self.core.lock().await;
        let core = guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        let input = parse_object(&input_json, "model input")?;
        let model = input
            .get("model")
            .and_then(Value::as_str)
            .ok_or_else(|| errors::invalid("model must be a string"))?;
        let provider = optional_string(&input, "providerId")?;
        let effort = match input.get("reasoningEffort") {
            None => None,
            Some(Value::Null) => Some(None),
            Some(Value::String(value)) => Some(Some(value.as_str())),
            _ => return Err(errors::invalid("reasoningEffort must be a string or null")),
        };
        let state = core
            .runtime
            .service()
            .set_model(&session_id, provider, model, effort)
            .await
            .map_err(errors::session)?;
        Ok(session_state_json(&state))
    }

    #[napi]
    pub async fn cancel_queued_input(&self, session_id: String, input_id: String) -> Result<bool> {
        let guard = self.core.lock().await;
        let core = guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        core.runtime
            .service()
            .cancel_queued_input(&session_id, &input_id)
            .await
            .map_err(errors::session)
    }

    #[napi]
    pub async fn clear_queue(&self, session_id: String) -> Result<u32> {
        let guard = self.core.lock().await;
        let core = guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        let count = core
            .runtime
            .service()
            .clear_queue(&session_id)
            .await
            .map_err(errors::session)?;
        u32::try_from(count).map_err(|_| errors::internal("queue count overflow"))
    }

    #[napi]
    pub async fn configure(&self, session_id: String, input_json: String) -> Result<String> {
        let guard = self.core.lock().await;
        let core = guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        let input = parse_object(&input_json, "configuration")?;
        let skills = match input.get("skills") {
            None => None,
            Some(Value::Array(values)) => Some(values.clone()),
            _ => return Err(errors::invalid("skills must be an array")),
        };
        let trusted = match input.get("workspaceTrusted") {
            None => None,
            Some(Value::Bool(value)) => Some(*value),
            _ => return Err(errors::invalid("workspaceTrusted must be a boolean")),
        };
        let state = core
            .runtime
            .service()
            .configure(&session_id, skills, input.get("context").cloned(), trusted)
            .await
            .map_err(errors::session)?;
        Ok(session_state_json(&state))
    }

    /// Delete the chat scope behind a session, straight through the store
    /// engine: an unknown chat fails `not_found`, and while any task in the
    /// scope is non-terminal the engine refuses atomically with `chat_busy`
    /// (never a delete that silently cancels live work). The binding adds no
    /// policy of its own.
    #[napi]
    pub async fn delete_chat(&self, session_id: String) -> Result<()> {
        let guard = self.core.lock().await;
        let core = guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        delete_chat(core.runtime.host().store(), &session_id).await
    }

    /// Hide the chat from the default listing; a named pass-through over the
    /// store's `archived` field, answering with the updated record JSON.
    #[napi]
    pub async fn archive_chat(&self, session_id: String) -> Result<String> {
        let guard = self.core.lock().await;
        let core = guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        set_chat_archived(core.runtime.host().store(), &session_id, true).await
    }

    /// The inverse of `archiveChat`: un-hide the chat, answering with the
    /// updated record JSON. An unknown chat fails `not_found`.
    #[napi]
    pub async fn unarchive_chat(&self, session_id: String) -> Result<String> {
        let guard = self.core.lock().await;
        let core = guard
            .as_ref()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        set_chat_archived(core.runtime.host().store(), &session_id, false).await
    }
}

/// The engine half of `delete_chat`, testable without a Node host: only the
/// store's own refusal (unknown chat, busy scope) crosses as an error.
pub(super) async fn delete_chat(store: &SqliteStore, chat_id: &str) -> Result<()> {
    store.delete_chat(chat_id).await.map_err(errors::store)
}

/// The engine half of `archive_chat`/`unarchive_chat`: one field on the
/// chat record, updated by the store; the returned record serializes verbatim.
pub(super) async fn set_chat_archived(
    store: &SqliteStore,
    chat_id: &str,
    archived: bool,
) -> Result<String> {
    let chat = store
        .update_chat(chat_id, None, None, Some(archived), SystemRuntime.now_ms())
        .await
        .map_err(errors::store)?;
    serde_json::to_string(&chat).map_err(|_| errors::internal("chat serialization"))
}

fn optional_string<'a>(input: &'a Value, key: &str) -> Result<Option<&'a str>> {
    match input.get(key) {
        None => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        _ => Err(errors::invalid(format!("{key} must be a string"))),
    }
}

#[cfg(test)]
mod tests {
    use super::{delete_chat, set_chat_archived};
    use agentkit_store::{SqliteStore, SubmitTask};
    use serde_json::{Value, json};

    const NOW: i64 = 1_760_000_000_000;

    fn store() -> SqliteStore {
        SqliteStore::memory(16).expect("in-memory store")
    }

    fn code(error: napi::Error) -> String {
        serde_json::from_str::<Value>(&error.reason).expect("error payload is JSON")["code"]
            .as_str()
            .expect("payload carries a code")
            .to_owned()
    }

    fn submit_input(task_id: &str, scope_id: &str) -> SubmitTask {
        SubmitTask {
            task_id: task_id.to_owned(),
            kind: "chat_turn".to_owned(),
            scope_id: scope_id.to_owned(),
            payload: json!({}),
            priority: 0,
            now: NOW,
            available_at: None,
            depends_on: vec![],
            parent_task_id: None,
            exclusive_scope: false,
        }
    }

    #[tokio::test]
    async fn archive_and_unarchive_persist_the_flag_and_reject_unknown_chats() {
        let store = store();
        store
            .create_chat("c1", None, json!({}), NOW)
            .await
            .expect("chat created");
        let archived: Value =
            serde_json::from_str(&set_chat_archived(&store, "c1", true).await.unwrap())
                .expect("record JSON");
        assert_eq!(archived["id"], "c1");
        assert_eq!(archived["archived"], true);
        assert!(
            store.get_chat("c1").await.unwrap().expect("chat").archived,
            "the flag is persisted in the record"
        );
        assert!(
            store.list_chats(false).await.unwrap().is_empty(),
            "the default listing excludes archived chats"
        );
        assert_eq!(
            store.list_chats(true).await.unwrap().len(),
            1,
            "the archived listing includes it"
        );
        let restored: Value =
            serde_json::from_str(&set_chat_archived(&store, "c1", false).await.unwrap())
                .expect("record JSON");
        assert_eq!(restored["archived"], false);
        assert_eq!(
            code(
                set_chat_archived(&store, "missing", true)
                    .await
                    .unwrap_err()
            ),
            "not_found"
        );
        assert_eq!(
            code(
                set_chat_archived(&store, "missing", false)
                    .await
                    .unwrap_err()
            ),
            "not_found"
        );
    }

    #[tokio::test]
    async fn delete_removes_the_scope_refusing_busy_and_unknown_chats() {
        let store = store();
        store
            .create_chat("c1", None, json!({}), NOW)
            .await
            .expect("chat created");
        store
            .submit(submit_input("t1", "c1"))
            .await
            .expect("task queued");
        assert_eq!(
            code(delete_chat(&store, "c1").await.unwrap_err()),
            "chat_busy",
            "live work in the scope is never silently deleted"
        );
        store
            .cancel_task("t1", NOW)
            .await
            .expect("queued task cancels");
        delete_chat(&store, "c1")
            .await
            .expect("delete settles then succeeds");
        assert!(
            store.get_chat("c1").await.unwrap().is_none(),
            "the chat is gone"
        );
        assert!(
            store.get_task("t1").await.unwrap().is_none(),
            "the scope's tasks are deleted with it"
        );
        store
            .create_chat("c1", None, json!({}), NOW)
            .await
            .expect("the same id recreates a fresh aggregate, not the deleted one");
        assert_eq!(
            code(delete_chat(&store, "missing").await.unwrap_err()),
            "not_found"
        );
    }
}
