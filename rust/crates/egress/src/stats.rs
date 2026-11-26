//! Counters in place of logs (NET-9): numbers only, never content.

use std::sync::atomic::{AtomicU64, Ordering};

/// A snapshot of one proxy's counters since it started.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EgressStats {
    /// Connections accepted on the socket.
    pub connections: u64,
    /// Tunnels currently holding a slot (resolving, connecting or open).
    pub open_tunnels: u64,
    /// Tunnels established: a CONNECT answered `200` whose TLS ClientHello
    /// named the granted host, or a plain HTTP request forwarded.
    pub tunnels_established: u64,
    /// Denials recorded, including ones the buffer has since dropped.
    pub denials: u64,
    /// Requests answered `400` or `408`, closed before a full head, or
    /// CONNECT tunnels the client closed before a whole first TLS record.
    pub bad_requests: u64,
    /// Granted names that did not resolve in time or to any address.
    pub resolve_failures: u64,
    /// Upstream connects that failed or timed out.
    pub connect_failures: u64,
    /// Tunnels closed because their grant was revoked (NET-6).
    pub closed_revoked: u64,
    /// Tunnels closed at their lifetime bound.
    pub closed_lifetime: u64,
    /// Tunnels closed at their byte bound.
    pub closed_bytes: u64,
    /// Bytes forwarded from the guest to upstream.
    pub bytes_up: u64,
    /// Bytes forwarded from upstream to the guest.
    pub bytes_down: u64,
    /// Failed `accept` calls (each followed by a short pause).
    pub accept_errors: u64,
}

/// Live counters; `open_tunnels` and `denials` are read elsewhere.
#[derive(Debug, Default)]
pub(crate) struct Counters {
    pub(crate) connections: AtomicU64,
    pub(crate) tunnels_established: AtomicU64,
    pub(crate) bad_requests: AtomicU64,
    pub(crate) resolve_failures: AtomicU64,
    pub(crate) connect_failures: AtomicU64,
    pub(crate) closed_revoked: AtomicU64,
    pub(crate) closed_lifetime: AtomicU64,
    pub(crate) closed_bytes: AtomicU64,
    pub(crate) bytes_up: AtomicU64,
    pub(crate) bytes_down: AtomicU64,
    pub(crate) accept_errors: AtomicU64,
}

/// Add one.
pub(crate) fn bump(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

impl Counters {
    pub(crate) fn snapshot(&self, open_tunnels: u64, denials: u64) -> EgressStats {
        let read = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        EgressStats {
            connections: read(&self.connections),
            open_tunnels,
            tunnels_established: read(&self.tunnels_established),
            denials,
            bad_requests: read(&self.bad_requests),
            resolve_failures: read(&self.resolve_failures),
            connect_failures: read(&self.connect_failures),
            closed_revoked: read(&self.closed_revoked),
            closed_lifetime: read(&self.closed_lifetime),
            closed_bytes: read(&self.closed_bytes),
            bytes_up: read(&self.bytes_up),
            bytes_down: read(&self.bytes_down),
            accept_errors: read(&self.accept_errors),
        }
    }
}
