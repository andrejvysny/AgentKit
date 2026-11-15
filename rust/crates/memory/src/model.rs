//! Domain types: the record, its enums and the filters the repository port
//! speaks in. The shapes mirror the spec's `memory_records` and
//! `memory_events` columns; wire strings are snake_case.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ids::{RecordId, Timestamp};

/// A record's text is one statement of at most this many characters.
pub const MAX_TEXT_CHARS: usize = 600;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeKind {
    User,
    Project,
    Agent,
}

/// Where a record applies. `id` is `owner`, a project id or an agent id.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Scope {
    pub kind: ScopeKind,
    pub id: String,
}

impl Scope {
    pub fn new(kind: ScopeKind, id: impl Into<String>) -> Self {
        Self {
            kind,
            id: id.into(),
        }
    }

    pub fn user() -> Self {
        Self::new(ScopeKind::User, "owner")
    }

    pub fn project(id: impl Into<String>) -> Self {
        Self::new(ScopeKind::Project, id)
    }

    pub fn agent(id: impl Into<String>) -> Self {
        Self::new(ScopeKind::Agent, id)
    }

    /// The partition key shared with the retrieval index:
    /// `scope_kind:scope_id`.
    pub fn key(&self) -> String {
        let kind = match self.kind {
            ScopeKind::User => "user",
            ScopeKind::Project => "project",
            ScopeKind::Agent => "agent",
        };
        format!("{kind}:{}", self.id)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Fact,
    Preference,
    Person,
    Routine,
    Lesson,
    SkillRef,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Active,
    Proposed,
    Rejected,
    Retracted,
}

impl Status {
    /// Legal status moves. `Active` and `Proposed` can be retracted;
    /// `Proposed` is approved or rejected; `Rejected` and `Retracted` are
    /// terminal (only a purge removes them).
    pub fn can_transition_to(self, to: Status) -> bool {
        matches!(
            (self, to),
            (Status::Proposed, Status::Active | Status::Rejected)
                | (Status::Active | Status::Proposed, Status::Retracted)
        )
    }

    /// The audit event a transition into `to` records.
    pub fn event_into(to: Status) -> EventKind {
        match to {
            Status::Active => EventKind::Approved,
            Status::Proposed => EventKind::Proposed,
            Status::Rejected => EventKind::Rejected,
            Status::Retracted => EventKind::Retracted,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    OwnerExplicit,
    OwnerStatement,
    AgentProposal,
    ConsolidationProposal,
    Import,
}

impl Origin {
    /// Only the owner's own words reach `Active` directly; agent and
    /// consolidation output waits as a proposal.
    pub fn default_status(self) -> Status {
        match self {
            Origin::OwnerExplicit | Origin::OwnerStatement | Origin::Import => Status::Active,
            Origin::AgentProposal | Origin::ConsolidationProposal => Status::Proposed,
        }
    }

    /// The actor credited in the audit trail for a record of this origin.
    pub fn actor(self) -> Actor {
        match self {
            Origin::OwnerExplicit => Actor::Owner,
            Origin::OwnerStatement => Actor::Extractor,
            Origin::AgentProposal => Actor::Agent,
            Origin::ConsolidationProposal | Origin::Import => Actor::System,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    Captured,
    Proposed,
    Approved,
    Rejected,
    Edited,
    Superseded,
    Retracted,
    Pinned,
    Unpinned,
    Purged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Actor {
    Owner,
    Extractor,
    Agent,
    System,
}

/// What a retrieval item belongs to. `Memory` is live; the rest are
/// reserved for later phases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Corpus {
    Memory,
    Pages,
    Chat,
    Mail,
}

/// A stored memory record (one `memory_records` row).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub id: RecordId,
    pub scope: Scope,
    pub kind: Kind,
    pub text: String,
    pub lang: Option<String>,
    pub status: Status,
    pub origin: Origin,
    pub evidence_quote: Option<String>,
    pub evidence_chat_id: Option<String>,
    pub evidence_msg_id: Option<String>,
    pub evidence_sha256: Option<String>,
    pub approved_at: Option<Timestamp>,
    pub valid_from: Timestamp,
    /// `None` while current.
    pub valid_to: Option<Timestamp>,
    pub supersedes: Option<RecordId>,
    pub superseded_by: Option<RecordId>,
    pub is_core: bool,
    pub core_order: Option<i64>,
    pub created_at: Timestamp,
    pub created_by_task: Option<String>,
    pub last_used_at: Option<Timestamp>,
    pub use_count: u64,
}

/// A record to insert. The repository mints the id; links and counters
/// start empty.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewRecord {
    pub scope: Scope,
    pub kind: Kind,
    pub text: String,
    pub lang: Option<String>,
    pub status: Status,
    pub origin: Origin,
    pub evidence_quote: Option<String>,
    pub evidence_chat_id: Option<String>,
    pub evidence_msg_id: Option<String>,
    pub evidence_sha256: Option<String>,
    pub approved_at: Option<Timestamp>,
    pub valid_from: Timestamp,
    pub is_core: bool,
    pub core_order: Option<i64>,
    pub created_at: Timestamp,
    pub created_by_task: Option<String>,
}

impl NewRecord {
    /// A record valid from `at`, with the status its origin implies.
    pub fn new(
        scope: Scope,
        kind: Kind,
        text: impl Into<String>,
        origin: Origin,
        at: Timestamp,
    ) -> Self {
        Self {
            scope,
            kind,
            text: text.into(),
            lang: None,
            status: origin.default_status(),
            origin,
            evidence_quote: None,
            evidence_chat_id: None,
            evidence_msg_id: None,
            evidence_sha256: None,
            approved_at: None,
            valid_from: at,
            is_core: false,
            core_order: None,
            created_at: at,
            created_by_task: None,
        }
    }

    /// Shape checks every adapter shares: non-blank text of at most
    /// [`MAX_TEXT_CHARS`] characters, and only `Active` or `Proposed` as an
    /// initial status. The write gate's content rules live above the port.
    pub fn validate(&self) -> Result<(), crate::MemoryError> {
        if self.text.trim().is_empty() {
            return Err(crate::MemoryError::InvalidInput(
                "record text is empty".to_owned(),
            ));
        }
        if self.text.chars().count() > MAX_TEXT_CHARS {
            return Err(crate::MemoryError::InvalidInput(format!(
                "record text exceeds {MAX_TEXT_CHARS} characters"
            )));
        }
        if !matches!(self.status, Status::Active | Status::Proposed) {
            return Err(crate::MemoryError::InvalidInput(
                "a record starts active or proposed".to_owned(),
            ));
        }
        Ok(())
    }

    /// The stored form under `id`, with links and counters empty.
    pub fn into_record(self, id: RecordId) -> Record {
        Record {
            id,
            scope: self.scope,
            kind: self.kind,
            text: self.text,
            lang: self.lang,
            status: self.status,
            origin: self.origin,
            evidence_quote: self.evidence_quote,
            evidence_chat_id: self.evidence_chat_id,
            evidence_msg_id: self.evidence_msg_id,
            evidence_sha256: self.evidence_sha256,
            approved_at: self.approved_at,
            valid_from: self.valid_from,
            valid_to: None,
            supersedes: None,
            superseded_by: None,
            is_core: self.is_core,
            core_order: self.core_order,
            created_at: self.created_at,
            created_by_task: self.created_by_task,
            last_used_at: None,
            use_count: 0,
        }
    }
}

/// One row of the append-only audit trail. `detail` never holds record
/// text, so a purge leaves none behind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryEvent {
    pub seq: u64,
    pub record_id: RecordId,
    pub kind: EventKind,
    pub actor: Actor,
    pub detail: Option<Value>,
    pub at: Timestamp,
}

/// Which records a listing returns. Empty vectors mean "any".
///
/// Validity: by default only current records (`valid_to` unset). `as_of`
/// returns what was valid at that instant; `include_superseded` returns
/// history as well and ignores `as_of`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecordFilter {
    pub scopes: Vec<Scope>,
    pub kinds: Vec<Kind>,
    pub statuses: Vec<Status>,
    pub as_of: Option<Timestamp>,
    pub include_superseded: bool,
    pub is_core: Option<bool>,
}

impl RecordFilter {
    /// The reference semantics of the filter; SQL adapters must agree.
    pub fn matches(&self, record: &Record) -> bool {
        if !self.scopes.is_empty() && !self.scopes.contains(&record.scope) {
            return false;
        }
        if !self.kinds.is_empty() && !self.kinds.contains(&record.kind) {
            return false;
        }
        if !self.statuses.is_empty() && !self.statuses.contains(&record.status) {
            return false;
        }
        if self.is_core.is_some_and(|core| core != record.is_core) {
            return false;
        }
        if self.include_superseded {
            return true;
        }
        match self.as_of {
            Some(at) => record.valid_from <= at && record.valid_to.is_none_or(|to| to > at),
            None => record.valid_to.is_none(),
        }
    }
}

/// Offset paging over a stable order (`created_at`, then id).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Page {
    pub offset: u32,
    pub limit: u32,
}

impl Page {
    pub fn new(offset: u32, limit: u32) -> Self {
        Self { offset, limit }
    }

    pub fn first(limit: u32) -> Self {
        Self { offset: 0, limit }
    }

    /// Everything, for callers that never page.
    pub fn all() -> Self {
        Self {
            offset: 0,
            limit: u32::MAX,
        }
    }
}

/// What a purge removed, per table, and how many rows for the purged ids
/// the same store still holds afterwards (the verification the spec asks
/// for; zero when clean). `neighbour_events_scrubbed` counts events of
/// other records whose `detail` named a purged id (the supersession links).
/// A purge is clean only with no residue and no WAL pages left pending.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PurgeReport {
    pub rows_removed: BTreeMap<String, u64>,
    pub neighbour_events_scrubbed: u64,
    pub residue: u64,
    /// The WAL checkpoint after the purge ran and finished (stores without a
    /// WAL report `false` and `wal_pages_pending == 0`).
    #[serde(default)]
    pub wal_checkpointed: bool,
    /// WAL pages the post-purge checkpoint could not fold back (a reader
    /// held them, or the checkpoint failed: then 1). Earlier page images of
    /// the purged content may still sit in the `-wal` file; not clean.
    #[serde(default)]
    pub wal_pages_pending: u64,
}

impl PurgeReport {
    pub fn is_clean(&self) -> bool {
        self.residue == 0 && self.wal_pages_pending == 0
    }

    pub fn removed(&self, table: &str) -> u64 {
        self.rows_removed.get(table).copied().unwrap_or(0)
    }

    /// Fold another store's report into this one (rows per table add up).
    pub fn merge(&mut self, other: PurgeReport) {
        for (table, rows) in other.rows_removed {
            *self.rows_removed.entry(table).or_default() += rows;
        }
        self.neighbour_events_scrubbed += other.neighbour_events_scrubbed;
        self.residue += other.residue;
        self.wal_checkpointed |= other.wal_checkpointed;
        self.wal_pages_pending += other.wal_pages_pending;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(valid_from: i64, valid_to: Option<i64>) -> Record {
        let mut new = NewRecord::new(
            Scope::user(),
            Kind::Fact,
            "x",
            Origin::OwnerExplicit,
            Timestamp(valid_from),
        );
        new.valid_from = Timestamp(valid_from);
        let mut record = new.into_record(RecordId::new());
        record.valid_to = valid_to.map(Timestamp);
        record
    }

    #[test]
    fn transitions_are_closed() {
        assert!(Status::Proposed.can_transition_to(Status::Active));
        assert!(Status::Active.can_transition_to(Status::Retracted));
        assert!(!Status::Retracted.can_transition_to(Status::Active));
        assert!(!Status::Rejected.can_transition_to(Status::Active));
        assert!(!Status::Active.can_transition_to(Status::Proposed));
    }

    #[test]
    fn filter_validity_modes() {
        let old = record(10, Some(20));
        let current = record(20, None);
        let default = RecordFilter::default();
        assert!(!default.matches(&old) && default.matches(&current));
        let at_15 = RecordFilter {
            as_of: Some(Timestamp(15)),
            ..Default::default()
        };
        assert!(at_15.matches(&old) && !at_15.matches(&current));
        let history = RecordFilter {
            include_superseded: true,
            ..Default::default()
        };
        assert!(history.matches(&old) && history.matches(&current));
    }

    #[test]
    fn validate_rejects_blank_and_oversized_text() {
        let mut new = NewRecord::new(
            Scope::user(),
            Kind::Fact,
            " ",
            Origin::OwnerExplicit,
            Timestamp(1),
        );
        assert!(new.validate().is_err());
        new.text = "a".repeat(MAX_TEXT_CHARS + 1);
        assert!(new.validate().is_err());
        new.text = "a".repeat(MAX_TEXT_CHARS);
        assert!(new.validate().is_ok());
    }
}
