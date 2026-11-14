//! Selected managed content is frozen before turn admission, outside chat history.
use crate::{RepositoryFault, SessionBackend, SessionFault, SessionsError, SessionsResult};
use agentkit_core::ChatMessage;
use agentkit_kernel::sha256_hex;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[async_trait]
pub trait ContentAuthority<E: RepositoryFault>: Send + Sync {
    /// Host-selected, authorized connector identities. User content grants nothing.
    async fn authorized_connectors(
        &self,
        chat_id: &str,
        cancel: CancellationToken,
    ) -> SessionsResult<Vec<String>, E>;
    /// Hosts with account-scoped grants return their exact selected grant identities.
    async fn authority_identity(
        &self,
        _chat_id: &str,
        required_connectors: &[String],
        _cancel: CancellationToken,
    ) -> SessionsResult<Value, E> {
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

pub async fn freeze_run_content<R: SessionBackend>(
    store: &R,
    selection: Value,
    chat_id: &str,
    authority: Option<&Arc<dyn ContentAuthority<SessionFault<R>>>>,
) -> SessionsResult<Value, SessionFault<R>> {
    let selection: RunContentSelection = serde_json::from_value(selection)
        .map_err(|e| SessionsError::InvalidContentSelectionDetail(e.to_string()))?;
    if selection.skills.len() > 20
        || selection
            .skills
            .iter()
            .any(|s| s.id.is_empty() || s.revision.is_empty())
    {
        return Err(SessionsError::InvalidContentSelection);
    }
    let cancel = CancellationToken::new();
    let _cancel_on_drop = cancel.clone().drop_guard();
    let selected = serde_json::to_value(&selection).expect("selection");
    let resolved = match store
        .resolve_run_content(selected.clone(), vec![], false)
        .await
    {
        Ok(resolved) => resolved,
        // The selection names managed content the host has not authorized
        // yet: ask the host for its grants and resolve once more.
        Err(error) if error.selected_connector_required() => {
            let grants = match authority {
                Some(authority) => tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    authority.authorized_connectors(chat_id, cancel.clone()),
                )
                .await
                .map_err(|_| SessionsError::ContentAuthorityTimeout)??,
                None => vec![],
            };
            store
                .resolve_run_content(selected, grants, false)
                .await
                .map_err(SessionsError::Store)?
        }
        Err(error) => return Err(SessionsError::Store(error)),
    };
    let authorized_connectors: Vec<String> =
        serde_json::from_value(resolved["authorizedConnectors"].clone())
            .map_err(|_| SessionsError::InvalidContentAuthority)?;
    let authority_identity = match authority {
        Some(authority) if !authorized_connectors.is_empty() => tokio::time::timeout(
            std::time::Duration::from_secs(10),
            authority.authority_identity(chat_id, &authorized_connectors, cancel.clone()),
        )
        .await
        .map_err(|_| SessionsError::ContentAuthorityTimeout)??,
        _ => serde_json::json!(authorized_connectors),
    };
    let mut messages = Vec::new();
    for message in resolved["messages"].as_array().into_iter().flatten() {
        messages.push(ChatMessage::text(
            message["role"]
                .as_str()
                .ok_or_else(|| SessionsError::InvalidContentMessage)?,
            message["content"]
                .as_str()
                .ok_or_else(|| SessionsError::InvalidContentMessage)?,
        ));
    }
    for block in resolved["blocks"].as_array().into_iter().flatten() {
        messages.push(ChatMessage::text(
            "system",
            block["content"]
                .as_str()
                .ok_or_else(|| SessionsError::InvalidContentBlock)?,
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
    serde_json::to_value(snapshot).map_err(SessionsError::Json)
}

pub async fn selected_messages<R: SessionBackend>(
    context: &agentkit_engine::ExecutionContext<R>,
    authority: Option<&Arc<dyn ContentAuthority<SessionFault<R>>>>,
) -> SessionsResult<Vec<ChatMessage>, SessionFault<R>> {
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
            return Err(SessionsError::ContentSnapshotRequired);
        }
        return Ok(vec![]);
    };
    let snapshot = validate_frozen_run_content(value).map_err(SessionsError::from_faultless)?;
    // Recheck authorization at every dispatch; frozen text never freezes a revoked grant.
    let cancel = context.cancellation.child_token();
    let _cancel_on_drop = cancel.clone().drop_guard();
    let authorized = match authority {
        Some(authority) if !snapshot.authorized_connectors.is_empty() => tokio::select! {
            _ = context.cancellation.cancelled() => return Err(SessionsError::Cancelled),
            result = tokio::time::timeout(std::time::Duration::from_secs(10), authority.authorized_connectors(&context.task.scope_id, cancel.clone())) => result.map_err(|_| SessionsError::ContentAuthorityTimeout)??,
        },
        _ => vec![],
    };
    if snapshot
        .authorized_connectors
        .iter()
        .any(|id| !authorized.contains(id))
    {
        return Err(SessionsError::ContentAuthorityRevoked);
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
        .map_err(|_| SessionsError::ContentAuthorityTimeout)??,
        _ => serde_json::json!(snapshot.authorized_connectors),
    };
    if identity != snapshot.authority_identity {
        return Err(SessionsError::ContentAuthorityChanged);
    }
    Ok(snapshot.messages)
}

/// Checks serialized identity and content integrity without granting dispatch authority.
pub fn validate_frozen_run_content(value: &Value) -> SessionsResult<FrozenRunContent> {
    let snapshot: FrozenRunContent = serde_json::from_value(value.clone())
        .map_err(|e| SessionsError::InvalidContentSnapshot(e.to_string()))?;
    if snapshot.digest != snapshot_digest(&snapshot) {
        return Err(SessionsError::ContentSnapshotDigestMismatch);
    }
    Ok(snapshot)
}

fn snapshot_digest(snapshot: &FrozenRunContent) -> String {
    let value = serde_json::json!({"selection":snapshot.selection,"authorizedConnectors":snapshot.authorized_connectors,"authorityIdentity":snapshot.authority_identity,"messages":snapshot.messages});
    sha256_hex(serde_json::to_vec(&value).expect("content serialization"))
}
