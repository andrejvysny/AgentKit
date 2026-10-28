//! Feature-selected facade for the independent Rust SDK.
//!
//! The default surface contains only portable contracts. Native execution,
//! persistence, HTTP, MCP and platform credential storage are explicit features.

#[cfg(feature = "client")]
pub use agentkit_client as client;
#[cfg(feature = "runtime")]
pub use agentkit_connections as connections;
pub use agentkit_contracts as contracts;
#[cfg(feature = "runtime")]
pub use agentkit_core as core;
#[cfg(feature = "durable")]
pub use agentkit_host as host;
#[cfg(feature = "mcp")]
pub use agentkit_mcp as mcp;
#[cfg(feature = "durable")]
pub use agentkit_store as store;
/// The confined coding tool pack (`coding` namespace contributor and guard).
#[cfg(feature = "coding")]
pub use agentkit_toolpacks_coding as toolpacks_coding;
#[cfg(feature = "http")]
pub use agentkit_transport_http as http;
