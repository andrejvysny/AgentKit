//! Feature-selected facade for the independent Rust SDK.
//!
//! The default surface contains only portable contracts. Native execution,
//! persistence, HTTP, MCP and platform credential storage are explicit features.

#[cfg(feature = "durable")]
pub use agentkit_chat as chat;
#[cfg(feature = "client")]
pub use agentkit_client as client;
#[cfg(feature = "runtime")]
pub use agentkit_connections as connections;
pub use agentkit_contracts as contracts;
#[cfg(feature = "runtime")]
pub use agentkit_core as core;
#[cfg(feature = "durable")]
pub use agentkit_engine as engine;
#[cfg(feature = "durable")]
pub use agentkit_governance as governance;
#[cfg(feature = "http")]
pub use agentkit_http as http;
#[cfg(feature = "mcp")]
pub use agentkit_mcp as mcp;
/// OPTIONAL standalone facade for driving one native Chat turn without the
/// durable host (AGENTKIT-117). No task/attempt store, no leases, no
/// approval governance, no MCP broker: those authorities stay in
/// `agentkit-engine` / `agentkit-store`.
#[cfg(feature = "claude")]
pub mod native_chat;
#[cfg(feature = "durable")]
pub use agentkit_providers as providers;
#[cfg(feature = "durable")]
pub use agentkit_sessions as sessions;
#[cfg(feature = "durable")]
pub use agentkit_store as store;
/// The confined coding tool pack (`coding` namespace contributor and guard).
#[cfg(feature = "coding")]
pub use agentkit_toolpacks_coding as toolpacks_coding;
