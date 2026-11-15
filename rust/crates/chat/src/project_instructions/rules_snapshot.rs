//! The session's frozen project rules (roadmap 1.4, ADR 0028 S3, ADR 0036).
//!
//! The first turn reads `AGENTS.md` / `CLAUDE.md` once; the text and its
//! hash are recorded in the chat metadata and that snapshot is S3 for the
//! rest of the session, so the bytes (and `rules_digest`) never move. Each
//! later turn re-reads the files cheaply (size + mtime, then hash) and a
//! change becomes a `project_rules_changed` note of the next turn context.
//! The snapshot is re-frozen, adopting the current files, when the notes
//! since the last freeze pass [`REFREEZE_DIFF_BYTES`], or when
//! [`refreeze_project_rules`] is called (explicit reload, compaction), and
//! when the workspace's trust changed (the S3 notice differs).
//!
//! Every write is one store transaction that re-reads the chat, checks the
//! lease fence and compare-and-sets on the record it was computed from, so a
//! concurrent refreeze or turn is never overwritten by a stale record.

use agentkit_kernel::sha256_hex;
use agentkit_store::SqliteStore;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{
    MAX_INSTRUCTION_BYTES, PROJECT_INSTRUCTION_FILES, ProjectInstructionsSource,
    ProjectInstructionsStat, UNTRUSTED_NOTICE, diff::unified_diff, emit_warnings,
    read_instruction_files, render_block, rules_workspace, unreadable_warning,
};
use crate::turn_context::{TurnNote, TurnNoteKind, with_turn_note, without_turn_notes};
use crate::{ChatError, ChatExecutorOptions, ChatResult, ExecutionContext};

/// Chat metadata key holding the frozen rules record.
pub const PROJECT_RULES_METADATA_KEY: &str = "agentkit.session.projectRules";
/// Notes totalling more than this since the last freeze trigger a re-freeze.
pub const REFREEZE_DIFF_BYTES: usize = 8 * 1024;
/// A stat match is trusted only when the file was last modified this long
/// before it was read: a write inside the timestamp granularity could keep
/// both size and mtime.
const STAT_SETTLE_NS: i64 = 2_000_000_000;
/// The note for an edit that changed no line.
const WHITESPACE_ONLY: &str = "(whitespace or line-ending change only)";
/// Attempts of an explicit refreeze that keeps losing the race.
const REFREEZE_ATTEMPTS: usize = 3;

/// One file as last read.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SeenFile {
    name: String,
    text: String,
    hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    stat: Option<ProjectInstructionsStat>,
    checked_ms: i64,
}

/// The recorded rules: the frozen S3 text and what the files looked like at
/// the last turn.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RulesRecord {
    /// Bumped on every re-freeze.
    revision: u64,
    /// The S3 block; empty when the session has no rules.
    snapshot: String,
    /// `sha256` of `snapshot`.
    digest: String,
    /// Whether the snapshot was rendered for a trusted workspace; a change
    /// of trust re-freezes (the untrusted notice is part of S3).
    trusted: bool,
    /// Bytes of change notes queued since the snapshot was taken.
    diff_bytes: usize,
    files: Vec<SeenFile>,
    /// Files refused or unreadable at the last read, so a refusal is warned
    /// once per change of state and not on every turn.
    #[serde(default)]
    refused: Vec<String>,
}

impl RulesRecord {
    fn frozen(revision: u64, files: Vec<SeenFile>, trusted: bool, refused: Vec<String>) -> Self {
        let snapshot = render_block(
            files.iter().map(|f| (f.name.as_str(), f.text.as_str())),
            trusted,
        );
        Self {
            revision,
            digest: sha256_hex(&snapshot),
            snapshot,
            trusted,
            diff_bytes: 0,
            files,
            refused,
        }
    }

    fn block(&self) -> Option<String> {
        (!self.snapshot.is_empty()).then(|| self.snapshot.clone())
    }
}

/// What the chat metadata holds under [`PROJECT_RULES_METADATA_KEY`].
enum Stored {
    Absent,
    /// Present but not a record this version reads; `revision` is what could
    /// still be made out of it (0 when nothing).
    Unreadable {
        revision: u64,
    },
    Record(RulesRecord),
}

fn read_stored(metadata: &Value) -> Stored {
    let Some(raw) = metadata.get(PROJECT_RULES_METADATA_KEY) else {
        return Stored::Absent;
    };
    match serde_json::from_value(raw.clone()) {
        Ok(record) => Stored::Record(record),
        Err(_) => Stored::Unreadable {
            revision: raw.get("revision").and_then(Value::as_u64).unwrap_or(0),
        },
    }
}

/// What a re-read found for one file.
enum Observation {
    Same,
    Gone,
    /// Refused (symlink, special file) or unreadable: keep what we know.
    Refused,
    Now(SeenFile),
}

fn settled(stat: &ProjectInstructionsStat, checked_ms: i64) -> bool {
    checked_ms
        .saturating_mul(1_000_000)
        .saturating_sub(stat.mtime_ns)
        > STAT_SETTLE_NS
}

async fn observe(
    source: &dyn ProjectInstructionsSource,
    root: &str,
    name: &str,
    prior: Option<&SeenFile>,
    now_ms: i64,
) -> Observation {
    let stat = source.stat(root, name).await;
    if let (Some(prior), Some(stat)) = (prior, &stat)
        && prior.stat.as_ref() == Some(stat)
        && settled(stat, prior.checked_ms)
    {
        return Observation::Same;
    }
    match source.read(root, name, MAX_INSTRUCTION_BYTES).await {
        Err(_) => Observation::Refused,
        Ok(None) if prior.is_some() => Observation::Gone,
        Ok(None) => Observation::Same,
        Ok(Some(found)) => Observation::Now(SeenFile {
            name: name.to_owned(),
            hash: sha256_hex(&found.text),
            text: found.text,
            stat,
            checked_ms: now_ms,
        }),
    }
}

/// The note text for a change: hunks of a unified diff, or the whole file
/// when the diff would be longer than it.
fn change_text(old: Option<&str>, new: Option<&str>, trusted: bool) -> String {
    let body = match (old, new) {
        (_, None) => "(file removed)".to_owned(),
        (None, Some(new)) => format!("(file added)\n{new}"),
        (Some(old), Some(new)) if old.lines().eq(new.lines()) => WHITESPACE_ONLY.to_owned(),
        (Some(old), Some(new)) => match unified_diff(old, new) {
            Some(diff) if diff.len() <= new.len() => diff,
            _ => format!("(full text)\n{new}"),
        },
    };
    if trusted {
        body
    } else {
        format!("{UNTRUSTED_NOTICE}\n\n{body}")
    }
}

/// The record after re-reading every file, with the notes for what changed.
/// `refrozen` is true when the notes outgrew the budget, or the trust of the
/// workspace changed, and the record adopted the current files as its
/// snapshot (the notes are then moot). `warnings` are refusals that are new
/// since the last check.
struct Checked {
    record: RulesRecord,
    notes: Vec<TurnNote>,
    refrozen: bool,
    warnings: Vec<String>,
}

async fn check(
    source: &dyn ProjectInstructionsSource,
    root: &str,
    trusted: bool,
    prior: &RulesRecord,
    now_ms: i64,
) -> Checked {
    let mut files: Vec<SeenFile> = Vec::new();
    let mut notes: Vec<TurnNote> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut refused: Vec<String> = Vec::new();
    let mut diff_bytes = prior.diff_bytes;
    for name in PROJECT_INSTRUCTION_FILES {
        let before = prior.files.iter().find(|f| f.name == name);
        let (kept, old, new) = match observe(source, root, name, before, now_ms).await {
            Observation::Same => (before.cloned(), None, None),
            Observation::Gone => (None, before, None),
            Observation::Refused => {
                refused.push(name.to_owned());
                if !prior.refused.iter().any(|r| r == name) {
                    warnings.push(unreadable_warning(name));
                }
                (before.cloned(), None, None)
            }
            // Same bytes: keep the record as it was unless the stat moved or
            // has not settled yet, so an idle session writes nothing.
            Observation::Now(file) if before.is_some_and(|b| b.hash == file.hash) => {
                let kept = before.filter(|b| {
                    b.stat == file.stat && file.stat.is_none_or(|s| settled(&s, b.checked_ms))
                });
                (Some(kept.cloned().unwrap_or(file)), None, None)
            }
            Observation::Now(file) => (Some(file.clone()), before, Some(file)),
        };
        if old.is_some() || new.is_some() {
            let text = change_text(
                old.map(|f| f.text.as_str()),
                new.as_ref().map(|f| f.text.as_str()),
                trusted,
            );
            diff_bytes += text.len();
            notes.push(TurnNote::new(
                TurnNoteKind::ProjectRulesChanged,
                Some(name),
                &text,
            ));
        }
        files.extend(kept);
    }
    if diff_bytes > REFREEZE_DIFF_BYTES || prior.trusted != trusted {
        return Checked {
            record: RulesRecord::frozen(prior.revision + 1, files, trusted, refused),
            notes: vec![],
            refrozen: true,
            warnings,
        };
    }
    Checked {
        record: RulesRecord {
            diff_bytes,
            files,
            refused,
            ..prior.clone()
        },
        notes,
        refrozen: false,
        warnings,
    }
}

/// The files as they are now, the read warnings and the refused names.
async fn read_seen(
    source: &dyn ProjectInstructionsSource,
    root: &str,
    now_ms: i64,
) -> (Vec<SeenFile>, Vec<String>, Vec<String>) {
    let (found, warnings, refused) = read_instruction_files(source, root).await;
    let mut files = Vec::new();
    for (name, file) in found {
        files.push(SeenFile {
            name: name.to_owned(),
            hash: sha256_hex(&file.text),
            text: file.text,
            stat: source.stat(root, name).await,
            checked_ms: now_ms,
        });
    }
    let refused = refused.into_iter().map(str::to_owned).collect();
    (files, warnings, refused)
}

/// One write: the record, the notes to queue and whether it re-froze.
struct Update {
    record: RulesRecord,
    notes: Vec<TurnNote>,
    refrozen: bool,
}

/// Whether a write landed.
enum Persisted {
    Done,
    /// The stored record moved since it was read; nothing was written.
    Raced,
}

/// Writes `record` and queues `notes` in one transaction, when the chat still
/// holds `expected` under the record key (compare-and-set on the raw value, so
/// a concurrent refreeze or an unreadable record is never overwritten). The
/// metadata is taken fresh inside the transaction, so no other key is lost. A
/// re-freeze drops the notes still waiting: the new snapshot already says it.
/// `context` supplies the lease fence of a turn; an explicit reload has none.
async fn persist(
    store: &SqliteStore,
    context: Option<&ExecutionContext>,
    chat_id: &str,
    now_ms: i64,
    expected: Option<&Value>,
    update: Update,
) -> ChatResult<Persisted> {
    let tx = store.begin_transaction().await?;
    let view = tx.view();
    if let Some(context) = context {
        view.verify_fence(&context.fence, now_ms).await?;
    }
    let chat = view
        .get_chat(chat_id)
        .await?
        .ok_or(ChatError::SessionDisappeared)?;
    if chat.metadata.get(PROJECT_RULES_METADATA_KEY) != expected {
        return Ok(Persisted::Raced);
    }
    let mut metadata = chat.metadata.clone();
    if update.refrozen {
        metadata = without_turn_notes(&metadata, TurnNoteKind::ProjectRulesChanged);
    }
    for note in update.notes {
        metadata = with_turn_note(&metadata, note);
    }
    metadata[PROJECT_RULES_METADATA_KEY] =
        serde_json::to_value(&update.record).expect("rules record serializes");
    view.update_chat(chat_id, None, Some(metadata), None, now_ms)
        .await?;
    if let Some(context) = context {
        view.verify_fence(&context.fence, context.runtime.now_ms())
            .await?;
        context.check_cancelled()?;
    }
    tx.commit().await?;
    Ok(Persisted::Done)
}

/// The block of whatever record the chat holds now, after losing a race.
async fn current_block(
    store: &SqliteStore,
    chat_id: &str,
    fallback: Option<String>,
) -> ChatResult<Option<String>> {
    let chat = store.get_chat(chat_id).await?;
    Ok(match chat.map(|c| read_stored(&c.metadata)) {
        Some(Stored::Record(record)) => record.block(),
        _ => fallback,
    })
}

/// The S3 block of a session turn: the frozen snapshot, recorded on the
/// first turn and re-frozen only by [`check`]'s budget (or a trust change, or
/// a record that cannot be read). Changes since the last turn are queued as
/// notes for the turn context that follows.
pub(super) async fn turn_rules(
    source: &dyn ProjectInstructionsSource,
    context: &ExecutionContext,
    chat_id: &str,
    root: &str,
    trusted: bool,
) -> ChatResult<Option<String>> {
    let now_ms = context.runtime.now_ms();
    let chat = context.store.get_chat(chat_id).await?;
    let expected = chat
        .as_ref()
        .and_then(|chat| chat.metadata.get(PROJECT_RULES_METADATA_KEY).cloned());
    let stored = chat
        .as_ref()
        .map_or(Stored::Absent, |chat| read_stored(&chat.metadata));
    let mut unreadable = false;
    let (record, notes, refrozen, warnings) = match stored {
        Stored::Record(prior) => {
            let checked = check(source, root, trusted, &prior, now_ms).await;
            if checked.record == prior {
                return Ok(prior.block());
            }
            (
                checked.record,
                checked.notes,
                checked.refrozen,
                checked.warnings,
            )
        }
        Stored::Absent => {
            let (files, warnings, refused) = read_seen(source, root, now_ms).await;
            (
                RulesRecord::frozen(1, files, trusted, refused),
                vec![],
                false,
                warnings,
            )
        }
        // Do not restart at revision 1: the revision keeps counting so the
        // prefix record sees the new digest and reports the cache break.
        Stored::Unreadable { revision } => {
            let (files, warnings, refused) = read_seen(source, root, now_ms).await;
            unreadable = true;
            let record = RulesRecord::frozen(revision.max(1) + 1, files, trusted, refused);
            (record, vec![], true, warnings)
        }
    };
    let block = record.block();
    let update = Update {
        record,
        notes,
        refrozen,
    };
    match persist(
        &context.store,
        Some(context),
        chat_id,
        now_ms,
        expected.as_ref(),
        update,
    )
    .await?
    {
        Persisted::Done => {
            if unreadable {
                context
                    .emit(
                        "run.warning",
                        json!({
                            "code": "project_rules_record_unreadable",
                            "message": "The recorded project rules could not be read; the rules were frozen again.",
                        }),
                    )
                    .await?;
            }
            emit_warnings(context, &warnings).await?;
            Ok(block)
        }
        Persisted::Raced => current_block(&context.store, chat_id, block).await,
    }
}

/// Adopts the workspace's current rules as the session's frozen S3 and drops
/// the change notes still queued. Call it for an explicit reload and at
/// compaction (the history is rewritten then, so the prefix may move). The
/// next turn's prefix check sees the new `rules_digest` and records the
/// break (`context.cache_break`, reason `refreeze`). A file that is refused
/// now keeps its previously frozen text. Returns false when nothing needed
/// adopting: no frozen rules yet, no workspace, or no change.
pub async fn refreeze_project_rules(
    options: &ChatExecutorOptions,
    store: &SqliteStore,
    chat_id: &str,
    now_ms: i64,
) -> ChatResult<bool> {
    let Some(source) = options.project_instructions.clone() else {
        return Ok(false);
    };
    let Some((root, trusted)) = rules_workspace(options, store, chat_id).await? else {
        return Ok(false);
    };
    for _ in 0..REFREEZE_ATTEMPTS {
        let Some(chat) = store.get_chat(chat_id).await? else {
            return Ok(false);
        };
        let expected = chat.metadata.get(PROJECT_RULES_METADATA_KEY).cloned();
        let (revision, prior) = match read_stored(&chat.metadata) {
            Stored::Absent => return Ok(false),
            Stored::Unreadable { revision } => (revision.max(1), None),
            Stored::Record(prior) => (prior.revision, Some(prior)),
        };
        let (mut files, _, refused) = read_seen(source.as_ref(), &root, now_ms).await;
        if let Some(prior) = &prior {
            for name in &refused {
                files.extend(prior.files.iter().filter(|f| &f.name == name).cloned());
            }
        }
        files.sort_by_key(|f| PROJECT_INSTRUCTION_FILES.iter().position(|n| *n == f.name));
        let next = RulesRecord::frozen(revision + 1, files, trusted, refused);
        if let Some(prior) = &prior
            && next.digest == prior.digest
            && prior.diff_bytes == 0
            && prior.trusted == trusted
            && prior.refused == next.refused
        {
            return Ok(false);
        }
        let update = Update {
            record: next,
            notes: vec![],
            refrozen: true,
        };
        match persist(store, None, chat_id, now_ms, expected.as_ref(), update).await? {
            Persisted::Done => return Ok(true),
            Persisted::Raced => {}
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use async_trait::async_trait;

    use super::*;
    use crate::project_instructions::ProjectInstructionsText;

    /// `Ok(Some(text))`, `Ok(None)` (absent) or `Err` (refused) per name.
    #[derive(Default)]
    struct Stub(HashMap<&'static str, Option<&'static str>>);

    #[async_trait]
    impl ProjectInstructionsSource for Stub {
        async fn read(
            &self,
            _: &str,
            name: &str,
            _: usize,
        ) -> ChatResult<Option<ProjectInstructionsText>> {
            match self.0.get(name) {
                None => Ok(None),
                Some(None) => Err(ChatError::InstructionReaderStopped),
                Some(Some(text)) => Ok(Some(ProjectInstructionsText {
                    text: (*text).to_owned(),
                    truncated: false,
                })),
            }
        }
    }

    fn seen(text: &str) -> SeenFile {
        SeenFile {
            name: "AGENTS.md".into(),
            text: text.into(),
            hash: sha256_hex(text),
            stat: None,
            checked_ms: 0,
        }
    }

    fn frozen(text: &str, trusted: bool) -> RulesRecord {
        RulesRecord::frozen(1, vec![seen(text)], trusted, vec![])
    }

    #[tokio::test]
    async fn a_trust_change_refreezes_with_the_new_notice() {
        let source = Stub(HashMap::from([("AGENTS.md", Some("rule\n"))]));
        let checked = check(&source, "/r", false, &frozen("rule\n", true), 10).await;
        assert!(checked.refrozen);
        assert!(!checked.record.trusted);
        assert_eq!(checked.record.revision, 2);
        assert!(checked.record.snapshot.contains(UNTRUSTED_NOTICE));
        assert!(checked.notes.is_empty());
    }

    #[tokio::test]
    async fn an_edit_on_an_untrusted_workspace_carries_the_notice() {
        let source = Stub(HashMap::from([("AGENTS.md", Some("new\n"))]));
        let checked = check(&source, "/r", false, &frozen("old\n", false), 10).await;
        assert!(!checked.refrozen);
        assert!(checked.notes[0].text.starts_with(UNTRUSTED_NOTICE));
    }

    #[tokio::test]
    async fn a_whitespace_only_edit_is_a_small_note() {
        let big: String = (0..200).map(|n| format!("rule number {n}\n")).collect();
        let crlf = big.replace('\n', "\r\n");
        let leaked: &'static str = Box::leak(crlf.into_boxed_str());
        let source = Stub(HashMap::from([("AGENTS.md", Some(leaked))]));
        let checked = check(&source, "/r", true, &frozen(&big, true), 10).await;
        assert_eq!(checked.notes.len(), 1);
        assert_eq!(checked.notes[0].text, WHITESPACE_ONLY);
        assert_eq!(checked.record.diff_bytes, WHITESPACE_ONLY.len());
        assert!(!checked.refrozen);
    }

    #[tokio::test]
    async fn a_refusal_keeps_the_prior_text_and_warns_once() {
        let source = Stub(HashMap::from([("AGENTS.md", None)]));
        let prior = frozen("rule\n", true);
        let first = check(&source, "/r", true, &prior, 10).await;
        assert_eq!(first.record.files, prior.files);
        assert_eq!(first.record.refused, vec!["AGENTS.md"]);
        assert_eq!(first.warnings, vec![unreadable_warning("AGENTS.md")]);
        assert!(first.notes.is_empty());
        let second = check(&source, "/r", true, &first.record, 20).await;
        assert!(second.warnings.is_empty());
        assert_eq!(second.record, first.record);
    }

    #[test]
    fn mtime_arithmetic_saturates() {
        let stat = ProjectInstructionsStat {
            size: 1,
            mtime_ns: i64::MIN,
        };
        assert!(settled(&stat, i64::MAX));
    }

    #[tokio::test]
    async fn a_stale_record_does_not_overwrite_a_newer_one() {
        let store = SqliteStore::memory(64).expect("store");
        store
            .create_chat("c", None, json!({"other": 1}), 1)
            .await
            .unwrap();
        let first = frozen("one\n", true);
        let write = |record: RulesRecord| Update {
            record,
            notes: vec![],
            refrozen: false,
        };
        let done = persist(&store, None, "c", 2, None, write(first.clone()))
            .await
            .unwrap();
        assert!(matches!(done, Persisted::Done));
        // A second writer that read the chat before `first` landed loses.
        let stale = RulesRecord::frozen(1, vec![seen("two\n")], true, vec![]);
        let lost = persist(&store, None, "c", 3, None, write(stale))
            .await
            .unwrap();
        assert!(matches!(lost, Persisted::Raced));
        let metadata = store.get_chat("c").await.unwrap().unwrap().metadata;
        assert!(matches!(read_stored(&metadata), Stored::Record(r) if r == first));
        assert_eq!(metadata["other"], 1, "other keys are kept");
    }
}
