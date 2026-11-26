//! Host-side egress proxy of the Track B sandbox backend (ADR 0039): one
//! unix-socket listener per machine, HTTP CONNECT (and absolute-URI HTTP on
//! port 80) checked against the machine's grants and the public-address
//! rules of `agentkit-kernel::net`.
//!
//! Per connection, in order:
//!
//! 1. Read the request head within [`EgressConfig::head_timeout`] and
//!    [`EgressConfig::head_max_bytes`]. Accepted: `CONNECT host:port`, and
//!    an absolute `http://host[:80]/..` request (rewritten to origin form,
//!    `Host` replaced by the URI's host, proxy headers dropped,
//!    `Connection: close` forced, one request per connection: its
//!    `Content-Length` body is forwarded and nothing the client sends after
//!    it). `Transfer-Encoding` and anything else is answered `400` and
//!    closed.
//! 2. Policy: the exact normalised host and port must be granted (NET-1,
//!    NET-2, NET-4). An IP literal is matched as text, so a name grant
//!    never covers it. Refused: `403`, denial `not_granted`.
//! 3. Tunnel limit (NET-8). Refused: `503`, denial `limit`.
//! 4. Resolve the absolute name on the host (NET-3; no search domains),
//!    bounded by [`EgressConfig::connect_timeout`]; IP literals are not
//!    resolved.
//! 5. Drop every address `agentkit_kernel::is_blocked` refuses or
//!    [`EgressConfig::extra_blocked`] lists; connect to the first remaining
//!    one at the granted port. None left: `502`, denial `blocked_address`.
//!    Resolution or connect failure: `502`, counted but not a denial.
//! 6. CONNECT only: answer `200`, then read the client's first TLS record
//!    within the head timeout. It must be a ClientHello whose server name
//!    is the granted host (no name only for an IP-literal target).
//!    Otherwise the tunnel closes with denial `sni_mismatch` and nothing
//!    reaches upstream. The record is then forwarded first.
//! 7. Pipe both ways until both sides close, the grant is revoked (NET-6),
//!    or the lifetime or byte bound is reached (NET-8).
//!
//! Nothing is logged; [`EgressProxy::stats`] exposes counts only (NET-9).
//!
//! Known limit: a CONNECT tunnel is checked by host, port and the server
//! name of its first ClientHello, nothing later. The HTTP `Host` inside the
//! TLS session is not seen, nor the inner hello of Encrypted Client Hello
//! (the outer server name is the one checked), nor a second ClientHello
//! after a HelloRetryRequest; a server at the granted address that serves
//! another site for those reaches it. Every CONNECT tunnel must start with
//! TLS, so other protocols (SSH, for one) are refused through the proxy.

mod config;
mod connection;
mod denials;
mod error;
mod net;
mod ports;
mod proxy;
mod relay;
mod request;
mod sni;
mod stats;
mod target;

pub use config::EgressConfig;
pub use denials::{DenialBatch, DenialReason, DenialRecord};
pub use error::EgressError;
pub use net::IpNet;
pub use ports::{Connector, EgressPorts, Resolver, SystemResolver, TcpConnector};
pub use proxy::EgressProxy;
pub use stats::EgressStats;
pub use target::{Target, TargetError};
