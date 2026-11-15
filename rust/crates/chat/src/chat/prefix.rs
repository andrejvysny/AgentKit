//! The session prefix record (ADR 0028): the frozen head of the prompt
//! (S1-S4) is recorded in the session's chat metadata on its first turn and
//! compared on every later one. A difference replaces the record under a new
//! revision and is reported as a `run.warning`, because it rewrites bytes a
//! provider cache may hold.

use crate::{ChatError, ChatResult, ExecutionContext};
use agentkit_contracts::{AiSessionPrefix, Optional};
use agentkit_core::{
    Classification, CoreError, Dialect, Freeze, PrefixDigests, PromotionObserver, PromptLayout,
    ToolDefinition, ToolLane, ToolManifest, ToolRegistry, ToolSurface, cache_key, freeze, tools_digest,
};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::sync::Arc;

/// Prefix `extra` key holding the S2 catalog index the session froze with.
const INDEX_KEY: &str = "deferredIndex";

/// What deferred tools add to a turn's prefix record (roadmap 2.3).
#[derive(Clone, Copy, Default)]
pub(super) struct Deferral<'a> {
    pub classification: Option<&'a Classification>,
    /// The index text already placed in S2 this turn.
    pub index: Option<&'a str>,
    /// The provider's wire protocol, which picks the profile's dialect.
    pub lane: ToolLane,
}

/// The session's dialect and the registry's offered definitions as it
/// presents them (T3.1): the manifest, the deferred catalog and the S2 index
/// all hold these names. A session whose tool list was frozen before
/// dialects existed keeps its canonical names.
fn presented(
    metadata: &Value,
    prior: Option<&AiSessionPrefix>,
    registry: &ToolRegistry,
    lane: ToolLane,
) -> (Option<Dialect>, Vec<ToolDefinition>) {
    let profile = agentkit_sessions::read_model_profile(metadata);
    let dialect = agentkit_core::session_dialect(prior, profile.as_ref(), lane);
    let canonical = registry.definitions();
    // The session freezes the table it actually renders (a partial editor
    // set falls back to common forms), so offered calls translate back.
    let dialect = dialect.map(|dialect| dialect.effective_for(&canonical));
    let offered = match &dialect {
        Some(dialect) => dialect.render(&canonical),
        None => canonical,
    };
    (dialect, offered)
}

/// The classification of this session (T4.1): the host's own, else the one
/// the session's frozen model profile asks for (direct tools, visible-tool
/// bound, load mode), else, when a contributor can join late, the default.
/// A late contributor keeps the meta tools in the array even when every tool
/// is direct, because nothing else reaches the tools that join later.
pub(super) async fn classification_for(
    context: &ExecutionContext,
    chat_id: &str,
    host: Option<&Classification>,
    late: bool,
    lane: ToolLane,
    registry: &ToolRegistry,
) -> ChatResult<Option<Classification>> {
    let chat = context
        .store
        .get_chat(chat_id)
        .await?
        .ok_or(ChatError::SessionDisappeared)?;
    let prior = agentkit_sessions::read_session_prefix(&chat.metadata);
    let from_profile = agentkit_sessions::read_model_profile(&chat.metadata).map(|profile| {
        // The wire names the profile's direct tools are offered as follow the
        // table the session actually renders.
        let dialect = agentkit_core::session_dialect(prior.as_ref(), Some(&profile), lane)
            .map(|dialect| dialect.effective_for(&registry.definitions()));
        Classification::from_profile(&profile, dialect.as_ref())
    });
    let mut classification = host
        .cloned()
        .or(from_profile)
        .or_else(|| late.then(Classification::default));
    if let Some(classification) = &mut classification {
        classification.keep_meta |= late;
    }
    Ok(classification)
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
    let installed = Arc::get_mut(&mut registry).is_some_and(|registry| {
        registry.set_visible_bound(classification.visible_bound);
        registry
            .install_meta_tools_with(classification.load_mode)
            .is_ok()
    });
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
    lane: ToolLane,
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
    let (dialect, offered) = presented(&chat.metadata, prior.as_ref(), registry, lane);
    let surface = ToolSurface::reconcile_with(prior.as_ref(), &offered, Some(classification));
    let view = registry.toolset_view(&offered, dialect.as_ref());
    Ok(surface.index_with(&view))
}

/// Records a promotion (T4.1, promote mode) at the turn boundary: the
/// promoted tools join the session's manifest, which is re-stamped under the
/// next prefix revision, so later turns send them without another change.
pub(super) struct PromotionRecorder {
    pub context: ExecutionContext,
    pub chat_id: String,
    pub branch_id: String,
}

#[async_trait::async_trait]
impl PromotionObserver for PromotionRecorder {
    async fn promoted(
        &self,
        _toolset: &str,
        definitions: &[ToolDefinition],
    ) -> Result<Option<u64>, CoreError> {
        self.record(definitions)
            .await
            .map_err(|error| CoreError::new("promotion_failed", &error.to_string()))
    }
}

impl PromotionRecorder {
    async fn record(&self, definitions: &[ToolDefinition]) -> ChatResult<Option<u64>> {
        let chat = self
            .context
            .store
            .get_chat(&self.chat_id)
            .await?
            .ok_or(ChatError::SessionDisappeared)?;
        let Some(prior) = agentkit_sessions::read_session_prefix(&chat.metadata) else {
            return Ok(None);
        };
        if ToolManifest::from_prefix(&prior).is_none() {
            return Ok(None);
        }
        let current = ToolSurface::reconcile_with(Some(&prior), &[], None);
        let names: BTreeSet<&str> = definitions.iter().map(|d| d.name.as_str()).collect();
        let joined: Vec<ToolDefinition> = current
            .manifest
            .definitions()
            .iter()
            .chain(definitions)
            .cloned()
            .collect();
        let surface = ToolSurface {
            manifest: ToolManifest::build(&joined),
            deferred: current
                .deferred
                .into_iter()
                .filter(|d| !names.contains(d.name.as_str()))
                .collect(),
            added: vec![],
            unavailable: vec![],
            demoted: vec![],
            frozen_now: false,
        };
        let revision = prior.revision as u64 + 1;
        let mut next = prior.clone();
        surface.stamp(&mut next);
        next.revision = revision as f64;
        next.tools_digest = tools_digest(surface.manifest.definitions());
        next.cache_key = cache_key(&self.chat_id, &self.branch_id, revision);
        next.frozen_at_task = self.context.task.task_id.clone();
        let decision = Freeze::Refrozen {
            prefix: next,
            changed: vec!["tools"],
        };
        persist(&self.context, &self.chat_id, &decision).await?;
        Ok(Some(revision))
    }
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
    let (dialect, offered) = presented(&chat.metadata, prior.as_ref(), registry, deferral.lane);
    let surface = ToolSurface::reconcile_with(prior.as_ref(), &offered, deferral.classification);
    let frozen = surface.manifest.definitions().to_vec();
    match &dialect {
        Some(dialect) => {
            warn_collisions(context, dialect, registry).await?;
            registry.freeze_presented(frozen, surface.deferred_names(), dialect.clone());
        }
        None => {
            registry.freeze_surface(frozen, surface.deferred_names());
        }
    }
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
    let decision = stamp_dialect(decision, dialect.as_ref());
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
        // INV-3: every break of the cached prefix is a durable event of its own.
        let reason = if changed.as_slice() == ["params"] {
            "params"
        } else {
            "refreeze"
        };
        context
            .emit("context.cache_break", json!({ "reason": reason }))
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

/// The decision with the dialect version actually used recorded in its
/// prefix (the profile's own version may be one this build does not know).
fn stamp_dialect(mut decision: Freeze, dialect: Option<&Dialect>) -> Freeze {
    let Some(dialect) = dialect else {
        return decision;
    };
    match &mut decision {
        Freeze::Created(prefix) | Freeze::Unchanged(prefix) | Freeze::Refrozen { prefix, .. } => {
            prefix.dialect_version = Optional::Present(dialect.version.clone());
        }
    }
    decision
}

/// Two tools never share a name in one session. A tool from elsewhere that is
/// named like a wire name of this dialect would silently lose its place in the
/// manifest, so it is reported.
async fn warn_collisions(
    context: &ExecutionContext,
    dialect: &Dialect,
    registry: &ToolRegistry,
) -> ChatResult<()> {
    let names = dialect.collisions(&registry.definitions());
    if names.is_empty() {
        return Ok(());
    }
    context
        .emit(
            "run.warning",
            json!({
                "code": "tool_name_collision",
                "message": format!(
                    "More than one tool would be offered under the name(s) {}; only the first is in the session's tool list.",
                    names.join(", ")
                ),
                "tools": names,
            }),
        )
        .await?;
    Ok(())
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
    if !surface.demoted.is_empty() {
        context
            .emit(
                "run.warning",
                json!({
                    "code": "tool_deferred",
                    "reason": "visible_bound",
                    "message": format!(
                        "{} tool(s) did not fit the profile's visible-tool bound and are in the deferred catalog; call them through tool_call: {}.",
                        surface.demoted.len(),
                        surface.demoted.join(", ")
                    ),
                    "tools": surface.demoted,
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
