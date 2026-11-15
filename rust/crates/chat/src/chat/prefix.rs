//! The session prefix record (ADR 0028): the frozen head of the prompt
//! (S1-S4) is recorded in the session's chat metadata on its first turn and
//! compared on every later one. A difference replaces the record under a new
//! revision and is reported as a `run.warning`, because it rewrites bytes a
//! provider cache may hold.

use crate::{ChatError, ChatResult, ExecutionContext};
use agentkit_core::{
    Classification, Freeze, PrefixDigests, PromptLayout, ToolRegistry, ToolSurface, freeze,
};
use serde_json::{Value, json};
use std::sync::Arc;

/// Prefix `extra` key holding the S2 catalog index the session froze with.
const INDEX_KEY: &str = "deferredIndex";

/// What deferred tools add to a turn's prefix record (roadmap 2.3).
#[derive(Clone, Copy, Default)]
pub(super) struct Deferral<'a> {
    pub classification: Option<&'a Classification>,
    /// The index text already placed in S2 this turn.
    pub index: Option<&'a str>,
}

/// Bind the meta tools to `registry` and complete `classification` with the
/// registry's own loading hints. A registry that is shared cannot take new
/// tools, so it keeps every tool direct.
pub(super) fn with_deferral(
    mut registry: Arc<ToolRegistry>,
    classification: Option<&Classification>,
) -> (Arc<ToolRegistry>, Option<Classification>) {
    let Some(classification) = classification else {
        return (registry, None);
    };
    let installed = Arc::get_mut(&mut registry)
        .is_some_and(|registry| registry.install_meta_tools().is_ok());
    if !installed {
        return (registry, None);
    }
    let mut classification = classification.clone();
    classification.deferred_hints = registry.deferred_hints();
    (registry, Some(classification))
}

/// The S2 catalog index for this turn: the one the session froze with, or on
/// its first turn the one its classification produces. It must not change
/// when a late tool joins the catalog, or S2 would be rewritten.
pub(super) async fn deferred_index(
    context: &ExecutionContext,
    chat_id: &str,
    registry: &ToolRegistry,
    classification: Option<&Classification>,
) -> ChatResult<Option<String>> {
    let Some(classification) = classification else {
        return Ok(None);
    };
    let chat = context
        .store
        .get_chat(chat_id)
        .await?
        .ok_or(ChatError::SessionDisappeared)?;
    let prior = agentkit_sessions::read_session_prefix(&chat.metadata);
    if let Some(text) = prior
        .as_ref()
        .and_then(|prefix| prefix.extra.get(INDEX_KEY))
        .and_then(Value::as_str)
    {
        return Ok(Some(text.to_owned()));
    }
    let surface =
        ToolSurface::reconcile_with(prior.as_ref(), &registry.definitions(), Some(classification));
    Ok(surface.index())
}

/// Record or compare the prefix of this turn. `branch_id` feeds the cache
/// key and is empty for lanes without provider continuation.
///
/// S1 is the session's frozen tool manifest (roadmap 1.3): the registry's
/// offered definitions become the stored manifest, in name order, on the
/// first turn and are never changed by what contributors answer later.
pub(super) async fn record(
    context: &ExecutionContext,
    chat_id: &str,
    layout: &PromptLayout,
    registry: &ToolRegistry,
    params: &Value,
    branch_id: &str,
    deferral: Deferral<'_>,
) -> ChatResult<()> {
    let chat = context
        .store
        .get_chat(chat_id)
        .await?
        .ok_or(ChatError::SessionDisappeared)?;
    let prior = agentkit_sessions::read_session_prefix(&chat.metadata);
    let surface = ToolSurface::reconcile_with(
        prior.as_ref(),
        &registry.definitions(),
        deferral.classification,
    );
    registry.freeze_surface(
        surface.manifest.definitions().to_vec(),
        surface.deferred_names(),
    );
    registry.freeze_catalog(&surface.deferred);
    let digests = PrefixDigests::compute(layout, &registry.definitions(), params);
    let decision = stamped(
        freeze(
            prior.as_ref(),
            digests,
            chat_id,
            branch_id,
            &context.task.task_id,
        ),
        &surface,
    );
    let decision = stamp_profile(decision, &chat.metadata);
    let decision = stamp_index(decision, deferral.index);
    warn_tools(context, &surface).await?;
    // An unchanged head still persists a grown deferred catalog or a
    // manifest recorded for the first time on an older session.
    if matches!(&decision, Freeze::Unchanged(kept) if Some(kept) == prior.as_ref()) {
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

/// The decision with the session's frozen model profile (T2.1) recorded in
/// its prefix. A session without a profile is left as it was.
fn stamp_profile(mut decision: Freeze, metadata: &Value) -> Freeze {
    if let Some(profile) = agentkit_sessions::read_model_profile(metadata) {
        match &mut decision {
            Freeze::Created(prefix)
            | Freeze::Unchanged(prefix)
            | Freeze::Refrozen { prefix, .. } => {
                agentkit_core::stamp_model_profile(prefix, &profile);
            }
        }
    }
    decision
}

/// The decision with the S2 catalog index recorded in its prefix, so later
/// turns put the same bytes in S2.
fn stamp_index(mut decision: Freeze, index: Option<&str>) -> Freeze {
    let Some(index) = index else {
        return decision;
    };
    match &mut decision {
        Freeze::Created(prefix) | Freeze::Unchanged(prefix) | Freeze::Refrozen { prefix, .. } => {
            prefix
                .extra
                .insert(INDEX_KEY.to_owned(), Value::String(index.to_owned()));
        }
    }
    decision
}

/// The decision with the manifest, its hash and the deferred catalog written
/// into its prefix.
fn stamped(decision: Freeze, surface: &ToolSurface) -> Freeze {
    match decision {
        Freeze::Created(mut prefix) => {
            surface.stamp(&mut prefix);
            Freeze::Created(prefix)
        }
        Freeze::Unchanged(mut prefix) => {
            surface.stamp(&mut prefix);
            Freeze::Unchanged(prefix)
        }
        Freeze::Refrozen {
            mut prefix,
            changed,
        } => {
            surface.stamp(&mut prefix);
            Freeze::Refrozen { prefix, changed }
        }
    }
}

/// Nothing is silently absent: tools that joined the deferred catalog and
/// tools the manifest lists but no contributor offers are reported.
async fn warn_tools(context: &ExecutionContext, surface: &ToolSurface) -> ChatResult<()> {
    if !surface.added.is_empty() {
        context
            .emit(
                "run.warning",
                json!({
                    "code": "tool_deferred",
                    "message": format!(
                        "{} tool(s) joined after the session's tool list was fixed and are in the deferred catalog: {}.",
                        surface.added.len(),
                        surface.added.join(", ")
                    ),
                    "tools": surface.added,
                }),
            )
            .await?;
    }
    if !surface.unavailable.is_empty() {
        context
            .emit(
                "run.warning",
                json!({
                    "code": "tool_unavailable",
                    "message": format!(
                        "Tools in the session's tool list are not available this turn: {}.",
                        surface.unavailable.join(", ")
                    ),
                    "tools": surface.unavailable,
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
