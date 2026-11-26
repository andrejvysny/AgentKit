//! Limits of one proxy (NET-8). Every bound is per machine.

use std::time::Duration;

use crate::error::EgressError;
use crate::net::IpNet;

/// Bounds of one egress listener.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EgressConfig {
    /// Open tunnels at once, counted from the moment a granted request
    /// starts resolving. One more is refused with `503` and a `limit`
    /// denial.
    pub max_tunnels: usize,
    /// Connections without a tunnel slot: sending their request head or
    /// being answered a refusal. Further connections wait in the listen
    /// backlog, unread, until one ends. Open connections are therefore at
    /// most `max_pending + max_tunnels`.
    pub max_pending: usize,
    /// Longest life of a tunnel after it is established.
    pub tunnel_lifetime: Duration,
    /// Bytes a tunnel carries, both directions together, including bytes
    /// the client sent after its request head and the TLS ClientHello (the
    /// head itself is not counted). Never exceeded; the tunnel closes when
    /// it is reached.
    pub max_bytes_per_tunnel: u64,
    /// Time to receive a complete request head, and separately the TLS
    /// ClientHello of a CONNECT tunnel; also bounds writing a refusal back.
    pub head_timeout: Duration,
    /// Longest request head, terminator included.
    pub head_max_bytes: usize,
    /// Bounds name resolution and, separately, the upstream connect.
    pub connect_timeout: Duration,
    /// Denials kept for [`crate::EgressProxy::drain`]; the oldest are
    /// dropped first.
    pub denial_buffer: usize,
    /// Addresses refused in addition to `agentkit_kernel::is_blocked`,
    /// such as the host's own public addresses, which the kernel rule
    /// cannot know. An IPv4 block also refuses the IPv6 forms that embed
    /// its addresses (mapped, NAT64, 6to4). Refused like any other blocked
    /// address: `502`, denial `blocked_address`. Empty by default.
    pub extra_blocked: Vec<IpNet>,
}

impl Default for EgressConfig {
    fn default() -> Self {
        Self {
            max_tunnels: 32,
            max_pending: 32,
            tunnel_lifetime: Duration::from_secs(10 * 60),
            max_bytes_per_tunnel: 1 << 30,
            head_timeout: Duration::from_secs(10),
            head_max_bytes: 8 * 1024,
            connect_timeout: Duration::from_secs(10),
            denial_buffer: 256,
            extra_blocked: Vec::new(),
        }
    }
}

/// Shortest head limit that still fits a request line and a host header.
const HEAD_MIN_BYTES: usize = 64;
/// Longest head limit: every connection allocates its head buffer up front.
const HEAD_MAX_BYTES: usize = 1 << 20;
/// Most connection slots of either kind, and most kept denials.
const COUNT_MAX: usize = 1 << 16;

impl EgressConfig {
    /// Refuse a zero bound, which would make the proxy useless or end a
    /// tunnel before it starts, and a count large enough to exhaust memory.
    pub fn validate(&self) -> Result<(), EgressError> {
        let count_ok = |n: usize| (1..=COUNT_MAX).contains(&n);
        let checks = [
            (
                count_ok(self.max_tunnels),
                "max_tunnels must be between 1 and 65536",
            ),
            (
                count_ok(self.max_pending),
                "max_pending must be between 1 and 65536",
            ),
            (
                !self.tunnel_lifetime.is_zero(),
                "tunnel_lifetime must be positive",
            ),
            (
                self.max_bytes_per_tunnel > 0,
                "max_bytes_per_tunnel must be at least 1",
            ),
            (
                !self.head_timeout.is_zero(),
                "head_timeout must be positive",
            ),
            (
                (HEAD_MIN_BYTES..=HEAD_MAX_BYTES).contains(&self.head_max_bytes),
                "head_max_bytes must be between 64 and 1 MiB",
            ),
            (
                !self.connect_timeout.is_zero(),
                "connect_timeout must be positive",
            ),
            (
                count_ok(self.denial_buffer),
                "denial_buffer must be between 1 and 65536",
            ),
        ];
        match checks.iter().find(|(ok, _)| !ok) {
            Some((_, problem)) => Err(EgressError::InvalidConfig(problem)),
            None => Ok(()),
        }
    }
}
