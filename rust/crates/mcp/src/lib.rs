//! Native MCP adapters using the official SDK, constrained to AgentKit's TS revisions.
mod backend;
mod bridge;
mod client;
mod guards;
mod handler;
mod http_client;
mod json_boundary;
mod json_fields;
mod manager;
mod server;
mod stdio_client;

pub use backend::{Backend, CatalogTool, Features, SessionScope};
pub use bridge::{BridgeError, McpToolSet};
pub use client::{ClientOptions, McpClient, McpClientError};
pub use manager::{
    ConnectReport, ContributionReport, DefaultConnector, ManagerClock, McpClientManager,
    McpConnection, McpConnector, McpSecretResolver, Resilience, ServerConfig, SystemClock,
    TransportConfig,
};
pub use rmcp::model;
pub use server::{Authenticator, BearerAuth, McpServer, ServerOptions};

/// Frozen from @modelcontextprotocol/sdk 1.30.0 dist/esm/types.js.
pub const TS_PROTOCOL_VERSIONS: &[&str] = &[
    "2025-11-25",
    "2025-06-18",
    "2025-03-26",
    "2024-11-05",
    "2024-10-07",
];

fn protocol_versions() -> Vec<model::ProtocolVersion> {
    TS_PROTOCOL_VERSIONS
        .iter()
        .map(|version| {
            serde_json::from_value(serde_json::Value::String((*version).into()))
                .expect("frozen MCP protocol version")
        })
        .collect()
}
