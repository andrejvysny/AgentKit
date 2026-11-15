//! The session prefix record (ADR 0028): the frozen head of the prompt
//! (S1-S4) is recorded in the session's chat metadata on its first turn and
//! compared on every later one. A difference replaces the record under a new
//! revision and is reported as a `run.warning`, because it rewrites bytes a
//! provider cache may hold.

use crate::{ChatError, ChatResult, ExecutionContext};
use agentkit_core::{Freeze, PrefixDigests, PromptLayout, ToolDefinition, freeze};
use serde_json::{Value, json};

/// Record or compare the prefix of this turn. `branch_id` feeds the cache
/// key and is empty for lanes without provider continuation.
pub(super) async fn record(
    context: &ExecutionContext,
    chat_id: &str,
    layout: &PromptLayout,
    tools: &[ToolDefinition],
    params: &Value,
    branch_id: &str,
) -> ChatResult<()> {
    let chat = context
        .store
        .get_chat(chat_id)
        .await?
        .ok_or(ChatError::SessionDisappeared)?;
    let prior = agentkit_sessions::read_session_prefix(&chat.metadata);
    let digests = PrefixDigests::compute(layout, tools, params);
    let decision = freeze(
        prior.as_ref(),
        digests,
        chat_id,
        branch_id,
        &context.task.task_id,
    );
    if matches!(decision, Freeze::Unchanged(_)) {
        return Ok(());
    }
    persist(context, chat_id, &decision).await?;
    if let Freeze::Refrozen { prefix, changed } = &decision {
        let revision = prefix.revision as u64;
        context
            .emit(
                "run.warning",
                json!({
                    "code": "prefix_refrozen",
                    "message": format!(
                        "The cached prompt prefix changed ({}); provider caches restart from revision {}.",
                        changed.join(", "),
                        revision
                    ),
                    "revision": revision,
                    "changed": changed,
                }),
            )
            .await?;
    }
    Ok(())
}

async fn persist(context: &ExecutionContext, chat_id: &str, decision: &Freeze) -> ChatResult<()> {
    let tx = context.store.begin_transaction().await?;
    let view = tx.view();
    view.verify_fence(&context.fence, context.runtime.now_ms())
        .await?;
    let chat = view
        .get_chat(chat_id)
        .await?
        .ok_or(ChatError::SessionDisappeared)?;
    let metadata = agentkit_sessions::with_session_prefix(&chat.metadata, decision.prefix());
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
