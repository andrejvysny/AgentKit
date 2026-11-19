//! What a history rewrite carries with it (roadmap 2.5). Masking and
//! compaction change bytes the provider may have cached, and on the
//! provider-continuation lane they change bytes a durable checkpoint
//! replays. So a rewrite voids that checkpoint, and a compaction, which
//! breaks the cache anyway, also adopts what had been waiting for such a
//! moment: the project rules (S3) and the core memory block (S4).

use super::{history::ContinuationSession, masking, session_meta};
use crate::{AgentExecutorOptions, ChatResult, ExecutionContext, refreeze_project_rules};
use agentkit_kernel::sha256_hex;
use serde_json::{Value, json};

/// Chat metadata key: `[{ "anchor": message id, "generation": text }]`, the
/// generation each continuation checkpoint was created under, newest last.
/// The checkpoint record itself is validated field by field by the store, so
/// what it was created under is kept beside it, keyed by its anchor message.
const GENERATIONS_KEY: &str = "agentkit.session.continuationGenerations";
const GENERATIONS_KEPT: usize = 32;

/// Chat metadata key: the rewrite epoch whose cache break was announced last.
const EPOCH_KEY: &str = "agentkit.session.cacheBreakEpoch";

/// The persisted state a provider-facing history is built from, besides the
/// messages: the prefix revision, the stored masks that apply and the summary
/// that applies. A checkpoint is valid only for the generation it was created
/// under (CE-15), so a retry after a failed attempt, an edit, an invalidated
/// mask or masking switched off all find it stale without any per-attempt flag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Generation {
    pub revision: u64,
    /// Digest of the applied mask ids; empty when none apply.
    masks: String,
    /// Id of the applied summary; empty when none applies.
    summary: String,
}

impl Generation {
    pub(super) fn new(revision: u64, applied_masks: &[String], summary: Option<&str>) -> Self {
        let mut ids: Vec<&str> = applied_masks.iter().map(String::as_str).collect();
        ids.sort_unstable();
        ids.dedup();
        Self {
            revision,
            masks: if ids.is_empty() {
                String::new()
            } else {
                sha256_hex(ids.join(","))
            },
            summary: summary.unwrap_or_default().to_owned(),
        }
    }

    /// The text kept with a checkpoint.
    pub(super) fn tag(&self) -> String {
        format!("{}|{}|{}", self.revision, self.masks, self.summary)
    }

    /// What the masks and the summary alone say. The revision is announced by
    /// the refreeze or the compaction that moves it.
    fn epoch(&self) -> Value {
        json!({"masks": self.masks, "summary": self.summary})
    }

    /// Nothing rewrote the history: a checkpoint saved before generations were
    /// kept can only be trusted then.
    fn unrewritten(&self) -> bool {
        self.masks.is_empty() && self.summary.is_empty()
    }
}

/// The generation a checkpoint anchored at `anchor` was created under.
pub(super) fn stored_generation(metadata: &Value, anchor: &str) -> Option<String> {
    metadata[GENERATIONS_KEY]
        .as_array()?
        .iter()
        .rev()
        .find(|entry| entry["anchor"] == anchor)
        .and_then(|entry| entry["generation"].as_str())
        .map(str::to_owned)
}

/// Keeps the generation a checkpoint is being saved under.
pub(super) async fn record_generation(
    context: &ExecutionContext,
    chat_id: &str,
    anchor: &str,
    generation: &str,
) -> ChatResult<()> {
    let (anchor, generation) = (anchor.to_owned(), generation.to_owned());
    session_meta::update(context, chat_id, move |metadata| {
        let mut kept: Vec<Value> = metadata[GENERATIONS_KEY]
            .as_array()
            .cloned()
            .unwrap_or_default();
        kept.retain(|entry| entry["anchor"] != anchor.as_str());
        kept.push(json!({"anchor": anchor, "generation": generation}));
        let excess = kept.len().saturating_sub(GENERATIONS_KEPT);
        kept.drain(..excess);
        metadata[GENERATIONS_KEY] = Value::Array(kept);
    })
    .await
}

/// Voids the checkpoint when this request replays a history it does not
/// describe (CE-15): its generation is not the current one, or the request
/// rewrites the history now in a way no stored record carries (a trim, a
/// batch not yet in the generation it is saved under). The provider then
/// starts a new checkpoint from the full rewritten history. Returns whether a
/// checkpoint was discarded.
pub(super) fn discard_stale(
    session: &mut ContinuationSession,
    current: &Generation,
    rewrote_now: bool,
) -> bool {
    if session.state.is_none() {
        return false;
    }
    let valid = !rewrote_now
        && match &session.generation {
            Some(stored) => *stored == current.tag(),
            None => current.unrewritten(),
        };
    if !valid {
        session.state = None;
    }
    !valid
}

/// What a compaction adopted.
pub(super) struct Adopted {
    pub rules: bool,
    pub memory: bool,
}

/// Adopts the pending project-rule and core-memory changes. Neither emits
/// the cache break: the compaction owns the one event of this rewrite.
pub(super) async fn adopt_pending(
    options: &AgentExecutorOptions,
    context: &ExecutionContext,
    chat_id: &str,
) -> ChatResult<Adopted> {
    let now = context.runtime.now_ms();
    let rules = refreeze_project_rules(options, &context.store, chat_id, now).await?;
    let memory = match &options.core_memory_refreeze {
        Some(hook) => hook.refreeze(chat_id, now).await?,
        None => false,
    };
    Ok(Adopted { rules, memory })
}

/// What a compaction contributes to the request's cache break.
pub(super) struct Compaction<'a> {
    pub tokens: (u64, u64),
    pub adopted: &'a Adopted,
    pub revision: Option<u64>,
    pub checkpoint_reset: bool,
}

/// The one `context.cache_break` of a request (INV-3). A compaction owns it
/// whatever else was rewritten: the summary, a trim, a mask batch, the adopted
/// rules and memory, the new prefix revision, the voided checkpoint. A mask
/// batch alone owns it otherwise.
///
/// The event is also derived from persisted state: the masks and the summary
/// that apply form an epoch, the last epoch announced is kept in the chat
/// metadata, and an attempt that finds the two apart announces the difference
/// (a crash between the write of a rewrite and its event is healed by the next
/// attempt) unless this task already did, or masking itself reported the
/// break (masks invalidated or switched off).
pub(super) async fn emit_break(
    context: &ExecutionContext,
    chat_id: &str,
    generation: &Generation,
    masked: &masking::Outcome,
    compaction: Option<Compaction<'_>>,
) -> ChatResult<()> {
    let epoch = generation.epoch();
    let announced = context
        .store
        .get_chat(chat_id)
        .await?
        .and_then(|chat| chat.metadata.get(EPOCH_KEY).cloned())
        .unwrap_or_else(|| Generation::new(0, &[], None).epoch());
    let owed = announced != epoch;
    let data = if let Some(compaction) = compaction {
        let mut data = json!({
            "reason": "compact",
            "tokens_before": compaction.tokens.0,
            "tokens_after": compaction.tokens.1,
            "masked": masked.results_masked + masked.images_masked,
            "rules_refrozen": compaction.adopted.rules,
            "memory_refrozen": compaction.adopted.memory,
            "checkpoint_reset": compaction.checkpoint_reset,
        });
        if let Some(revision) = compaction.revision {
            data["revision"] = revision.into();
        }
        Some(data)
    } else if masked.rewrote() {
        Some(json!({
            "reason": "mask",
            "tokens_before": masked.tokens.0,
            "tokens_after": masked.tokens.1,
            "results_masked": masked.results_masked,
            "images_masked": masked.images_masked,
        }))
    } else if owed && !masked.reported_break && !announced_by_task(context, &epoch).await? {
        let reason = if announced["summary"] != epoch["summary"] {
            "compact"
        } else {
            "mask"
        };
        Some(json!({"reason": reason, "recovered": true}))
    } else {
        None
    };
    if let Some(mut data) = data {
        data["generation"] = generation.tag().into();
        data["epoch"] = epoch.clone();
        context.emit("context.cache_break", data).await?;
    }
    if owed {
        session_meta::update(context, chat_id, move |metadata| {
            metadata[EPOCH_KEY] = epoch;
        })
        .await?;
    }
    Ok(())
}

/// Whether an earlier attempt of this task already announced `epoch`.
async fn announced_by_task(context: &ExecutionContext, epoch: &Value) -> ChatResult<bool> {
    let events = context
        .store
        .events(&context.task.task_id, None, 1000)
        .await?;
    Ok(events.iter().any(|record| {
        record.event["type"] == "context.cache_break" && record.event["data"]["epoch"] == *epoch
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn generation(revision: u64, masks: &[&str], summary: Option<&str>) -> Generation {
        let masks: Vec<String> = masks.iter().map(|id| (*id).to_owned()).collect();
        Generation::new(revision, &masks, summary)
    }

    fn session(state: bool, stored: Option<&Generation>) -> ContinuationSession {
        ContinuationSession {
            state: state.then(|| json!({})),
            scope: agentkit_core::ContinuationScope {
                chat_id: "c".into(),
                branch_id: "b".into(),
            },
            generation: stored.map(Generation::tag),
        }
    }

    #[test]
    fn a_checkpoint_is_valid_only_for_the_generation_it_was_saved_under() {
        let saved = generation(2, &["a", "b"], Some("s1"));
        for (current, valid) in [
            (generation(2, &["b", "a"], Some("s1")), true),
            (generation(3, &["a", "b"], Some("s1")), false),
            (generation(2, &["a"], Some("s1")), false),
            (generation(2, &[], Some("s1")), false),
            (generation(2, &["a", "b"], Some("s2")), false),
            (generation(2, &["a", "b"], None), false),
        ] {
            let mut checkpoint = session(true, Some(&saved));
            assert_eq!(!discard_stale(&mut checkpoint, &current, false), valid);
            assert_eq!(checkpoint.state.is_some(), valid);
        }
    }

    #[test]
    fn a_rewrite_made_now_voids_even_a_matching_checkpoint() {
        let saved = generation(1, &[], None);
        let mut checkpoint = session(true, Some(&saved));
        assert!(discard_stale(&mut checkpoint, &saved, true));
        assert!(checkpoint.state.is_none());
        assert!(!discard_stale(&mut checkpoint, &saved, true), "none left");
    }

    #[test]
    fn a_checkpoint_without_a_generation_is_trusted_only_for_an_unrewritten_history() {
        let mut plain = session(true, None);
        assert!(!discard_stale(&mut plain, &generation(1, &[], None), false));
        let mut masked = session(true, None);
        assert!(discard_stale(
            &mut masked,
            &generation(1, &["a"], None),
            false
        ));
    }

    #[test]
    fn generations_are_kept_per_anchor() {
        let metadata = json!({GENERATIONS_KEY: [
            {"anchor": "m1", "generation": "g1"}, {"anchor": "m2", "generation": "g2"}
        ]});
        assert_eq!(stored_generation(&metadata, "m2").as_deref(), Some("g2"));
        assert_eq!(stored_generation(&metadata, "m3"), None);
        assert_eq!(stored_generation(&json!({}), "m1"), None);
    }
}
