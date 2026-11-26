//! Refused attempts (NET-5), kept in a bounded buffer and read by cursor.
//!
//! Only refusals by rule are denials: a target that is not granted, a name
//! whose every address is refused, the tunnel limit, and a CONNECT tunnel
//! whose TLS ClientHello does not name the granted host. A name that does
//! not resolve, a failed connect, a malformed request and a client that
//! closes before its ClientHello are not policy decisions; they are
//! answered on the connection (or closed) and counted in
//! [`crate::EgressStats`] only.

use std::collections::VecDeque;
use std::fmt;

/// Why an attempt was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DenialReason {
    /// The exact host and port has no grant (NET-1, NET-2).
    NotGranted,
    /// Granted, but every address it resolves to is local, private or
    /// otherwise not public (NET-3).
    BlockedAddress,
    /// Granted, but the machine already has its maximum of open tunnels
    /// (NET-8). Not a reason to ask for a grant.
    Limit,
    /// A granted CONNECT whose first bytes were not a TLS ClientHello with
    /// the granted host as its server name: another name (another site on
    /// the same address), none for a named target, not TLS, malformed or
    /// oversized, or nothing whole within the head timeout. The record
    /// names the CONNECT target, never the name the client sent (NET-9).
    SniMismatch,
}

impl DenialReason {
    /// Stable snake_case name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotGranted => "not_granted",
            Self::BlockedAddress => "blocked_address",
            Self::Limit => "limit",
            Self::SniMismatch => "sni_mismatch",
        }
    }
}

impl fmt::Display for DenialReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One refused attempt. Carries no request content (NET-9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DenialRecord {
    /// Position in the proxy's denial sequence, from 1, without gaps.
    pub seq: u64,
    /// Normalised requested host (see [`crate::Target`]).
    pub host: String,
    /// Requested port.
    pub port: u16,
    /// Epoch milliseconds, from the injected clock.
    pub at_ms: i64,
    /// Why it was refused.
    pub reason: DenialReason,
    /// The operation marked current when it was refused.
    pub operation_id: Option<String>,
}

/// Denials after a cursor.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DenialBatch {
    /// Kept denials with `seq` above the cursor, oldest first.
    pub denials: Vec<DenialRecord>,
    /// The latest `seq` recorded so far (0 before the first denial): pass
    /// it next time. Never lower than a cursor this proxy returned before.
    pub cursor: u64,
    /// Denials after the given cursor that were dropped from the full
    /// buffer before this read.
    pub missed: u64,
}

/// The bounded buffer behind [`crate::EgressProxy::drain`].
#[derive(Debug)]
pub(crate) struct DenialLog {
    capacity: usize,
    last_seq: u64,
    records: VecDeque<DenialRecord>,
}

impl DenialLog {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            capacity,
            last_seq: 0,
            records: VecDeque::new(),
        }
    }

    /// Append with the next sequence number, dropping the oldest when full.
    pub(crate) fn push(&mut self, mut record: DenialRecord) {
        self.last_seq += 1;
        record.seq = self.last_seq;
        if self.records.len() == self.capacity {
            self.records.pop_front();
        }
        self.records.push_back(record);
    }

    /// Kept records after `cursor`. Reading removes nothing, so a caller
    /// that lost a batch can read it again with its old cursor.
    pub(crate) fn after(&self, cursor: u64) -> DenialBatch {
        let denials: Vec<DenialRecord> = self
            .records
            .iter()
            .filter(|record| record.seq > cursor)
            .cloned()
            .collect();
        let first_kept = self.records.front().map_or(self.last_seq + 1, |r| r.seq);
        let missed = first_kept.saturating_sub(cursor.saturating_add(1));
        DenialBatch {
            denials,
            cursor: self.last_seq,
            missed,
        }
    }

    pub(crate) fn total(&self) -> u64 {
        self.last_seq
    }
}

#[cfg(test)]
mod tests;
