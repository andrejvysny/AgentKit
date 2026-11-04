//! Native LSP client, language-server lifecycle manager and AgentKit
//! code-intelligence tools for AgentKit.
//!
//! [`pool::LspPool`] owns one language-server process per
//! (workspace root, language) pair, shared by every agent bound to the
//! pool. [`tools::lsp_tools`] contributes the six read-only tools
//! (`lsp_definition`, `lsp_references`, `lsp_hover`, `lsp_symbols`,
//! `lsp_implementation`, `lsp_diagnostics`) over that pool.

mod discovery;
mod documents;
mod error;
mod jsonrpc;
mod pool;
mod positions;
mod server;
mod tools;
mod uri;

pub use discovery::{
    LANGUAGES, LanguageSpec, detect_language, language_for_extension, language_for_path,
    resolve_server_command,
};
pub use documents::{
    DocumentStore, SyncAction, normalize_diagnostics, normalize_diagnostics_with_text,
    resolve_confined,
};
pub use error::{
    CANCELLED, INVALID_FILE, INVALID_INPUT, PROTOCOL, REQUEST_TIMEOUT, SERVER_CRASHED,
    SERVER_ERROR, SERVER_NOT_FOUND, SERVER_UNAVAILABLE, STALE_RESULT,
};
pub use jsonrpc::{Connection, RpcErrorObject, RpcFailure};
pub use pool::{LspConfig, LspPool, ServerCommand};
pub use server::{ServerHandle, ServerProcess};
pub use tools::lsp_tools;
pub use uri::{path_to_uri, uri_to_path};
