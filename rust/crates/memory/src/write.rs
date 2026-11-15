//! The write paths of the service that package 3.3 adds: the agent's
//! proposal (W1 and W3) and the owner's actions on the review queue (approve,
//! reject, edit, retract, pin, unpin). Every path goes through
//! [`crate::gate`] first; the model's own judgment is never the check. W1
//! (an active record from the owner's own words) needs two facts found in
//! code: the record text is a verbatim span of the owner's typed words of the
//! turn, and those words ask to remember. The model's `owner_requested` flag
//! and any quote it offers are not consulted; the evidence is the span.

use std::sync::atomic::Ordering;

use sha2::{Digest, Sha256};

use crate::core_block::{
    CORE_BUDGET_TOKENS, CoreBlock, core_tokens, entry_tokens, render_core_block,
};
use crate::error::{MemoryError, MemoryResult};
use crate::gate::{self, QuoteCheck};
use crate::ids::{ItemId, RecordId, Timestamp};
use crate::model::{
    Actor, EventKind, Kind, NewRecord, Origin, Page, Record, RecordFilter, Scope, Status,
};
use crate::ports::CorePin;
use crate::retrieval::Searchability;
use crate::service::{MemoryService, REJECTED_RETENTION_MS, index_item};

/// A rationale is one short paragraph.
pub const MAX_RATIONALE_CHARS: usize = 300;

/// The owner's typed text of the turn a proposal was made in (never the turn
/// context, a tool result or a child's report), with where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerMessage {
    pub chat_id: String,
    pub message_id: String,
    pub text: String,
}

/// What the agent asks to remember.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proposal {
    pub scope: Scope,
    pub kind: Kind,
    pub text: String,
    pub rationale: String,
    /// The agent says the owner asked for it in this turn. Advisory: W1 is
    /// decided by [`gate::owner_span`] and [`gate::has_remember_request`].
    pub owner_requested: bool,
    /// The turn's owner message, when the host could find it.
    pub owner_turn: Option<OwnerMessage>,
    /// The run that made the proposal.
    pub task: Option<String>,
}

/// Why a request to write directly became a proposal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Downgrade {
    NoOwnerMessage,
    TextTooShort,
    TextNotOwnerWords,
    NoRememberRequest,
}

impl Downgrade {
    pub fn message(self) -> &'static str {
        match self {
            Self::NoOwnerMessage => "this turn has no owner message to check the text against",
            Self::TextTooShort => "the text is shorter than 12 characters",
            Self::TextNotOwnerWords => {
                "the text is not a verbatim span of the owner's message of this turn"
            }
            Self::NoRememberRequest => "the owner's message does not ask to remember anything",
        }
    }
}

/// What a proposal became.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProposeOutcome {
    /// W1: the record is the owner's own words, asked to be remembered; it is active.
    Active(RecordId),
    /// Waiting in the review queue; `why` says what stopped a direct write.
    Proposed {
        id: RecordId,
        why: Option<Downgrade>,
    },
    /// The same statement is already active or already waiting.
    Duplicate(RecordId),
    /// The owner rejected the same statement less than 90 days ago.
    Suppressed,
}

fn sha256_hex(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

fn refuse(refusal: gate::GateRefusal) -> MemoryError {
    MemoryError::InvalidInput(refusal.message())
}

fn current_and_active(record: &Record) -> bool {
    record.status == Status::Active && record.valid_to.is_none()
}

impl MemoryService {
    /// Take an agent's proposal through the gate. When the record text is a
    /// verbatim span of the owner's typed words of the turn and those words
    /// ask to remember (both checked in code), the record is written active
    /// (`owner_explicit`, the span as its evidence); otherwise it waits as a
    /// proposal, hidden from search and the core block.
    pub async fn propose(
        &self,
        proposal: Proposal,
        now: Timestamp,
    ) -> MemoryResult<ProposeOutcome> {
        gate::screen_text(&proposal.text).map_err(refuse)?;
        if proposal.rationale.chars().count() > MAX_RATIONALE_CHARS {
            return Err(MemoryError::InvalidInput(format!(
                "the rationale is longer than {MAX_RATIONALE_CHARS} characters"
            )));
        }
        if gate::contains_secret(&proposal.rationale) {
            return Err(refuse(gate::GateRefusal::Secret));
        }
        if let Some(seen) = self
            .already_known(&proposal.scope, &proposal.text, now)
            .await?
        {
            return Ok(seen);
        }
        let (span, why) = match &proposal.owner_turn {
            None => (None, Some(Downgrade::NoOwnerMessage)),
            Some(turn) => match gate::owner_span(&proposal.text, &turn.text) {
                Ok(span) => {
                    let why = (!gate::has_remember_request(&turn.text))
                        .then_some(Downgrade::NoRememberRequest);
                    (Some(span), why)
                }
                Err(QuoteCheck::TooShort) => (None, Some(Downgrade::TextTooShort)),
                Err(_) => (None, Some(Downgrade::TextNotOwnerWords)),
            },
        };
        // The evidence is stored whatever the text said, so it is screened too.
        if span.as_deref().is_some_and(gate::contains_secret) {
            return Err(refuse(gate::GateRefusal::Secret));
        }
        let direct = why.is_none();
        let origin = if direct {
            Origin::OwnerExplicit
        } else {
            Origin::AgentProposal
        };
        let mut record = NewRecord::new(
            proposal.scope,
            proposal.kind,
            proposal.text.trim(),
            origin,
            now,
        );
        if let (Some(span), Some(turn)) = (span, &proposal.owner_turn) {
            record.evidence_quote = Some(span);
            record.evidence_chat_id = Some(turn.chat_id.clone());
            record.evidence_msg_id = Some(turn.message_id.clone());
            record.evidence_sha256 = Some(sha256_hex(&turn.text));
        }
        if direct {
            record.approved_at = Some(now);
        }
        record.rationale = Some(proposal.rationale);
        record.created_by_task = proposal.task;
        let id = self.insert(record).await?;
        Ok(if direct {
            ProposeOutcome::Active(id)
        } else {
            ProposeOutcome::Proposed { id, why }
        })
    }

    /// The duplicate and repeat-suppression rules for a candidate: the same
    /// normalized statement already active or waiting in the scope, or
    /// rejected by the owner within [`REJECTED_RETENTION_MS`].
    async fn already_known(
        &self,
        scope: &Scope,
        text: &str,
        now: Timestamp,
    ) -> MemoryResult<Option<ProposeOutcome>> {
        let key = gate::normalized_key(text);
        let filter = RecordFilter {
            scopes: vec![scope.clone()],
            statuses: vec![Status::Active, Status::Proposed, Status::Rejected],
            ..Default::default()
        };
        for record in self.repository.list(filter, Page::all()).await? {
            if gate::normalized_key(&record.text) != key {
                continue;
            }
            match record.status {
                Status::Active | Status::Proposed => {
                    self.note_use(&[record.id], now);
                    return Ok(Some(ProposeOutcome::Duplicate(record.id)));
                }
                Status::Rejected => {
                    let events = self.repository.events(record.id).await?;
                    let rejected_at = events
                        .iter()
                        .rev()
                        .find(|event| event.kind == EventKind::Rejected)
                        .map_or(record.created_at, |event| event.at);
                    if now.0.saturating_sub(rejected_at.0) < REJECTED_RETENTION_MS {
                        return Ok(Some(ProposeOutcome::Suppressed));
                    }
                }
                Status::Retracted => {}
            }
        }
        Ok(None)
    }

    /// Approve a waiting proposal. With `edited` the owner's text is stored
    /// instead: a new active record replaces the proposal, which is retracted
    /// in the same transaction. The text passes the gate either way.
    pub async fn approve(
        &self,
        id: RecordId,
        edited: Option<&str>,
        now: Timestamp,
    ) -> MemoryResult<RecordId> {
        let record = self.get(id).await?.ok_or(MemoryError::NotFound(id))?;
        if record.status != Status::Proposed {
            return Err(MemoryError::InvalidTransition {
                from: record.status,
                to: Status::Active,
            });
        }
        let text = edited.map_or(record.text.as_str(), str::trim);
        gate::screen_text(text).map_err(refuse)?;
        if text == record.text {
            self.set_status(id, Status::Active, Actor::Owner, now)
                .await?;
            return Ok(id);
        }
        let mut new = NewRecord::new(record.scope.clone(), record.kind, text, record.origin, now);
        new.status = Status::Active;
        new.approved_at = Some(now);
        new.lang = record.lang.clone();
        new.created_by_task = record.created_by_task.clone();
        new.rationale = Some(format!("owner-edited approval of proposal {id}"));
        new.validate()?;
        let (scope, body) = (new.scope.clone(), new.text.clone());
        let valid = (new.valid_from, None);
        let _gate = self.write_gate.read().await;
        let mut tx = self.repository.begin().await?;
        let new_id = self.repository.insert(&mut tx, new).await?;
        self.repository
            .set_status(&mut tx, id, Status::Retracted, Actor::Owner, now)
            .await?;
        let mut index_tx = self.index.begin_index().await?;
        let item = index_item(new_id, &scope, &body, now, Searchability::Current, valid);
        self.index.upsert(&mut index_tx, &[item]).await?;
        self.repository
            .commit_with_index(tx, &*self.index, index_tx)
            .await?;
        self.index_hint.store(true, Ordering::Release);
        Ok(new_id)
    }

    /// Reject a waiting proposal; the same statement is suppressed for 90
    /// days.
    pub async fn reject(&self, id: RecordId, now: Timestamp) -> MemoryResult<()> {
        self.set_status(id, Status::Rejected, Actor::Owner, now)
            .await
    }

    /// Retract a record (the chip's Undo, or forget): active records leave
    /// search and the core block, a proposal leaves the queue. A purge
    /// follows 30 days later.
    pub async fn retract(&self, id: RecordId, actor: Actor, now: Timestamp) -> MemoryResult<()> {
        self.set_status(id, Status::Retracted, actor, now).await
    }

    /// The owner edits an active record: a new owner-stated record replaces
    /// it (supersession), keeping the pin and its place in the core block.
    pub async fn edit(&self, id: RecordId, text: &str, now: Timestamp) -> MemoryResult<RecordId> {
        let old = self.get(id).await?.ok_or(MemoryError::NotFound(id))?;
        if !current_and_active(&old) {
            return Err(MemoryError::InvalidTransition {
                from: old.status,
                to: Status::Active,
            });
        }
        let text = text.trim();
        gate::screen_text(text).map_err(refuse)?;
        let at = Timestamp(now.0.max(old.valid_from.0));
        let mut new = NewRecord::new(old.scope.clone(), old.kind, text, Origin::OwnerExplicit, at);
        new.lang = old.lang.clone();
        new.approved_at = Some(at);
        // The replacement keeps the pin only if it still fits: the pin is
        // applied in the same transaction, where the budget is re-checked
        // against the new text, not copied blindly from the old record.
        let pin = old.is_core.then_some(CorePin {
            order: old.core_order,
            max_tokens: CORE_BUDGET_TOKENS,
        });
        self.supersede_pinned(id, new, pin).await
    }

    /// Pin an active record to the core block of its scope. Refused when the
    /// block of the owner's scope and this record's scope would pass
    /// [`CORE_BUDGET_TOKENS`].
    pub async fn pin(&self, id: RecordId, now: Timestamp) -> MemoryResult<()> {
        let record = self.get(id).await?.ok_or(MemoryError::NotFound(id))?;
        if !current_and_active(&record) {
            return Err(MemoryError::InvalidInput(
                "only a current active record can be pinned".to_owned(),
            ));
        }
        if record.is_core {
            return Ok(());
        }
        let mut scopes = vec![Scope::user()];
        if record.scope != Scope::user() {
            scopes.push(record.scope.clone());
        }
        let pinned = self.repository.core_block(&scopes).await?;
        let would_use = core_tokens(&pinned) + entry_tokens(&record);
        if would_use > CORE_BUDGET_TOKENS {
            return Err(MemoryError::InvalidInput(format!(
                "pinning would use {would_use} of the {CORE_BUDGET_TOKENS} core-block tokens; unpin or shorten an entry first"
            )));
        }
        let order = pinned.iter().filter(|r| r.scope == record.scope).count() as i64;
        let mut tx = self.repository.begin().await?;
        self.repository
            .set_core(
                &mut tx,
                id,
                Some(CorePin {
                    order: Some(order),
                    max_tokens: CORE_BUDGET_TOKENS,
                }),
                Actor::Owner,
                now,
            )
            .await?;
        self.repository.commit(tx).await
    }

    /// Take a record out of the core block; it stays searchable.
    pub async fn unpin(&self, id: RecordId, now: Timestamp) -> MemoryResult<()> {
        let record = self.get(id).await?.ok_or(MemoryError::NotFound(id))?;
        if !record.is_core {
            return Ok(());
        }
        let mut tx = self.repository.begin().await?;
        self.repository
            .set_core(&mut tx, id, None, Actor::Owner, now)
            .await?;
        self.repository.commit(tx).await
    }

    /// The core block of `scopes`, rendered as S4 would carry it now.
    pub async fn core_block(&self, scopes: &[Scope]) -> MemoryResult<CoreBlock> {
        Ok(render_core_block(
            &self.repository.core_block(scopes).await?,
        ))
    }

    /// Estimated tokens the core block of `scopes` uses and the budget, for
    /// the Memory screen's fill meter.
    pub async fn core_fill(&self, scopes: &[Scope]) -> MemoryResult<(usize, usize)> {
        let pinned = self.repository.core_block(scopes).await?;
        Ok((core_tokens(&pinned), CORE_BUDGET_TOKENS))
    }

    /// The records the Memory screen lists, oldest first.
    pub async fn list(&self, filter: RecordFilter, page: Page) -> MemoryResult<Vec<Record>> {
        self.repository.list(filter, page).await
    }

    /// The record and the records it replaced or was replaced by, oldest
    /// first, at most 32 steps either way.
    pub async fn history(&self, id: RecordId) -> MemoryResult<Vec<Record>> {
        const STEPS: usize = 32;
        let record = self.get(id).await?.ok_or(MemoryError::NotFound(id))?;
        let mut chain = vec![record.clone()];
        let mut cursor = record.supersedes;
        while let Some(prev) = cursor.filter(|_| chain.len() <= STEPS) {
            let Some(found) = self.get(prev).await? else {
                break;
            };
            cursor = found.supersedes;
            chain.insert(0, found);
        }
        let mut cursor = record.superseded_by;
        while let Some(next) = cursor.filter(|_| chain.len() <= 2 * STEPS) {
            let Some(found) = self.get(next).await? else {
                break;
            };
            cursor = found.superseded_by;
            chain.push(found);
        }
        Ok(chain)
    }
}

/// An index item id is its record id.
pub(crate) fn record_of(item: &ItemId) -> Option<RecordId> {
    item.0.parse().ok()
}
