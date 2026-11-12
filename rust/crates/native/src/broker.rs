//! Broker ports and contracts for the native lane (AGENTKIT-116).
//!
//! The MCP implementation lives in `agentkit-native-broker`; the executor
//! only ever sees [`AttemptBrokerHandle`].

use crate::McpEndpointSpec;
use async_trait::async_trait;
use serde_json::Value;

/// One catalog entry frozen at attempt start.
#[derive(Clone, Debug)]
pub struct BrokerToolDefinition {
    /// Server-local name; Claude sees `mcp__<server>__<name>`.
    pub name: String,
    pub description: String,
    /// JSON Schema for arguments (validated by the authority).
    pub input_schema: Value,
    /// Host-owned effect classification; never inferred from vendor data.
    pub writes: bool,
}

/// Per-attempt resource bounds (AGENTKIT-116).
#[derive(Clone, Copy, Debug)]
pub struct BrokerLimits {
    pub max_calls: usize,
    pub max_args_bytes: usize,
    /// Complete serialized MCP result and result-evidence payload limit.
    /// Durable receipt identity/envelope has a separate complete-row cap
    /// of 2 MiB, enforced by the store lifecycle API.
    pub max_result_bytes: usize,
    pub call_timeout_ms: i64,
    /// Hard bound on `BrokerAuthority::authorize` (the human approval
    /// gate). Review R5: the approval window a product advertises must be
    /// COMPATIBLE with, and strictly below, the layers above it — the
    /// vendor MCP request budget must exceed this gate plus the effect
    /// timeout, and the consumer's displayed approval deadline must fit
    /// inside this budget so an answered approval never races an outer
    /// transport expiry.
    pub authorize_timeout_ms: i64,
}

impl Default for BrokerLimits {
    fn default() -> Self {
        BrokerLimits {
            max_calls: 64,
            max_args_bytes: 256 * 1024,
            max_result_bytes: 1024 * 1024,
            call_timeout_ms: 120_000,
            authorize_timeout_ms: BROKER_AUTHORIZE_BUDGET_MS,
        }
    }
}

/// The bridge's default human-gate budget (review R5). OneMind clamps its
/// advertised approval window under this minus a margin, and the vendor
/// MCP request budget (claude launch) exceeds this plus the effect
/// timeout — one coherent, compatible deadline chain.
pub const BROKER_AUTHORIZE_BUDGET_MS: i64 = 360_000;

/// What the native executor needs from a live per-attempt tool broker.
/// The MCP implementation lives in `agentkit-native-broker`.
#[async_trait]
pub trait AttemptBrokerHandle: Send + Sync {
    /// Endpoint the vendor runtime is told to connect to.
    fn spec(&self) -> McpEndpointSpec;
    /// Stop authenticating immediately and cancel live calls.
    fn revoke(&self);
    fn live_calls(&self) -> usize;
    fn parked_ms(&self) -> u64;
    async fn shutdown(&self);
}
