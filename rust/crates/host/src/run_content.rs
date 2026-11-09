//! Selected managed content is frozen before turn admission, outside chat history.
use crate::{ExecutionContext, HostError, HostResult};
use agentkit_core::ChatMessage;
use agentkit_kernel::sha256_hex;
use agentkit_store::SqliteStore;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[async_trait]
pub trait ContentAuthority: Send + Sync {
    /// Host-selected, authorized connector identities. User content grants nothing.
    async fn authorized_connectors(
        &self,
        chat_id: &str,
        cancel: CancellationToken,
    ) -> HostResult<Vec<String>>;
    /// Hosts with account-scoped grants return their exact selected grant identities.
    async fn authority_identity(
        &self,
        _chat_id: &str,
        required_connectors: &[String],
        _cancel: CancellationToken,
    ) -> HostResult<Value> {
        Ok(serde_json::json!(required_connectors))
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RunContentSelection {
    pub template: Option<TemplateSelection>,
    #[serde(default)]
    pub skills: Vec<crate::SkillSelection>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TemplateSelection {
    pub id: String,
    pub revision: String,
    pub arguments: serde_json::Map<String, Value>,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FrozenRunContent {
    pub selection: RunContentSelection,
    pub authorized_connectors: Vec<String>,
    pub authority_identity: Value,
    pub messages: Vec<ChatMessage>,
    #[serde(default)]
    pub digest: String,
}

pub async fn freeze_run_content(
    store: &SqliteStore,
    selection: Value,
    chat_id: &str,
    authority: Option<&Arc<dyn ContentAuthority>>,
) -> HostResult<Value> {
    let selection: RunContentSelection = serde_json::from_value(selection)
        .map_err(|e| HostError::Execution(format!("invalid_content_selection: {e}")))?;
    if selection.skills.len() > 20
        || selection
            .skills
            .iter()
            .any(|s| s.id.is_empty() || s.revision.is_empty())
    {
        return Err(HostError::Execution("invalid_content_selection".into()));
    }
    let cancel = CancellationToken::new();
    let _cancel_on_drop = cancel.clone().drop_guard();
    let selected = serde_json::to_value(&selection).expect("selection");
    let resolved = match store
        .resolve_run_content(selected.clone(), vec![], false)
        .await
    {
        Ok(resolved) => resolved,
        Err(agentkit_store::StoreError::InvalidInput(detail))
            if detail == "Skill requires a selected authorized connector." =>
        {
            let grants = match authority {
                Some(authority) => tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    authority.authorized_connectors(chat_id, cancel.clone()),
                )
                .await
                .map_err(|_| HostError::Execution("content_authority_timeout".into()))??,
                None => vec![],
            };
            store.resolve_run_content(selected, grants, false).await?
        }
        Err(error) => return Err(error.into()),
    };
    let authorized_connectors: Vec<String> =
        serde_json::from_value(resolved["authorizedConnectors"].clone())
            .map_err(|_| HostError::Execution("invalid_content_authority".into()))?;
    let authority_identity = match authority {
        Some(authority) if !authorized_connectors.is_empty() => tokio::time::timeout(
            std::time::Duration::from_secs(10),
            authority.authority_identity(chat_id, &authorized_connectors, cancel.clone()),
        )
        .await
        .map_err(|_| HostError::Execution("content_authority_timeout".into()))??,
        _ => serde_json::json!(authorized_connectors),
    };
    let mut messages = Vec::new();
    for message in resolved["messages"].as_array().into_iter().flatten() {
        messages.push(ChatMessage::text(
            message["role"]
                .as_str()
                .ok_or_else(|| HostError::Execution("invalid_content_message".into()))?,
            message["content"]
                .as_str()
                .ok_or_else(|| HostError::Execution("invalid_content_message".into()))?,
        ));
    }
    for block in resolved["blocks"].as_array().into_iter().flatten() {
        messages.push(ChatMessage::text(
            "system",
            block["content"]
                .as_str()
                .ok_or_else(|| HostError::Execution("invalid_content_block".into()))?,
        ));
    }
    let mut snapshot = FrozenRunContent {
        selection,
        authorized_connectors,
        authority_identity,
        messages,
        digest: String::new(),
    };
    snapshot.digest = snapshot_digest(&snapshot);
    serde_json::to_value(snapshot).map_err(|e| HostError::Execution(e.to_string()))
}

pub(crate) async fn selected_messages(
    context: &ExecutionContext,
    authority: Option<&Arc<dyn ContentAuthority>>,
) -> HostResult<Vec<ChatMessage>> {
    let Some(value) = context
        .task
        .payload
        .get("contentSnapshot")
        .filter(|v| !v.is_null())
    else {
        if context
            .task
            .payload
            .get("contentSelection")
            .is_some_and(|v| !v.is_null())
        {
            return Err(HostError::Execution("content_snapshot_required".into()));
        }
        return Ok(vec![]);
    };
    let snapshot = validate_frozen_run_content(value)?;
    // Recheck authorization at every dispatch; frozen text never freezes a revoked grant.
    let cancel = context.cancellation.child_token();
    let _cancel_on_drop = cancel.clone().drop_guard();
    let authorized = match authority {
        Some(authority) if !snapshot.authorized_connectors.is_empty() => tokio::select! {
            _ = context.cancellation.cancelled() => return Err(HostError::Cancelled),
            result = tokio::time::timeout(std::time::Duration::from_secs(10), authority.authorized_connectors(&context.task.scope_id, cancel.clone())) => result.map_err(|_| HostError::Execution("content_authority_timeout".into()))??,
        },
        _ => vec![],
    };
    if snapshot
        .authorized_connectors
        .iter()
        .any(|id| !authorized.contains(id))
    {
        return Err(HostError::Execution("content_authority_revoked".into()));
    }
    let identity = match authority {
        Some(authority) if !snapshot.authorized_connectors.is_empty() => tokio::time::timeout(
            std::time::Duration::from_secs(10),
            authority.authority_identity(
                &context.task.scope_id,
                &snapshot.authorized_connectors,
                cancel.clone(),
            ),
        )
        .await
        .map_err(|_| HostError::Execution("content_authority_timeout".into()))??,
        _ => serde_json::json!(snapshot.authorized_connectors),
    };
    if identity != snapshot.authority_identity {
        return Err(HostError::Execution("content_authority_changed".into()));
    }
    Ok(snapshot.messages)
}

/// Checks serialized identity and content integrity without granting dispatch authority.
pub fn validate_frozen_run_content(value: &Value) -> HostResult<FrozenRunContent> {
    let snapshot: FrozenRunContent = serde_json::from_value(value.clone())
        .map_err(|e| HostError::Execution(format!("invalid_content_snapshot: {e}")))?;
    if snapshot.digest != snapshot_digest(&snapshot) {
        return Err(HostError::Execution(
            "content_snapshot_digest_mismatch".into(),
        ));
    }
    Ok(snapshot)
}

fn snapshot_digest(snapshot: &FrozenRunContent) -> String {
    let value = serde_json::json!({"selection":snapshot.selection,"authorizedConnectors":snapshot.authorized_connectors,"authorityIdentity":snapshot.authority_identity,"messages":snapshot.messages});
    sha256_hex(serde_json::to_vec(&value).expect("content serialization"))
}
