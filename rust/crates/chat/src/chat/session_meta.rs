//! Fenced read-modify-write of one chat's metadata, for the small durable
//! records the context pipeline keeps per session (token calibration, the
//! mask-disabled notice, the continuation generations).

use crate::{ChatError, ChatResult, ExecutionContext};
use serde_json::Value;

/// Applies `change` to the chat's current metadata inside one transaction
/// that checks the lease fence, so no other key is lost and a turn that no
/// longer owns the lease writes nothing.
pub(super) async fn update(
    context: &ExecutionContext,
    chat_id: &str,
    change: impl FnOnce(&mut Value),
) -> ChatResult<()> {
    let tx = context.store.begin_transaction().await?;
    let view = tx.view();
    view.verify_fence(&context.fence, context.runtime.now_ms())
        .await?;
    let chat = view
        .get_chat(chat_id)
        .await?
        .ok_or(ChatError::SessionDisappeared)?;
    let mut metadata = chat.metadata;
    if !metadata.is_object() {
        metadata = Value::Object(Default::default());
    }
    change(&mut metadata);
    view.update_chat(
        chat_id,
        None,
        Some(metadata),
        None,
        context.runtime.now_ms(),
    )
    .await?;
    view.verify_fence(&context.fence, context.runtime.now_ms())
        .await?;
    context.check_cancelled()?;
    tx.commit().await?;
    Ok(())
}
