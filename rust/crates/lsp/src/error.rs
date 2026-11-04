//! The crate's diagnostic codes. Every failure surfaced to a tool caller
//! is an [`agentkit_core::CoreError`] with one of these codes.

use agentkit_core::CoreError;

pub const SERVER_NOT_FOUND: &str = "lsp_server_not_found";
pub const SERVER_CRASHED: &str = "lsp_server_crashed";
pub const SERVER_UNAVAILABLE: &str = "lsp_server_unavailable";
pub const REQUEST_TIMEOUT: &str = "lsp_request_timeout";
pub const INVALID_FILE: &str = "lsp_invalid_file";
pub const STALE_RESULT: &str = "lsp_stale_result";
pub const CANCELLED: &str = "cancelled";
pub const PROTOCOL: &str = "lsp_protocol_error";
pub const INVALID_INPUT: &str = "lsp_invalid_input";
pub const SERVER_ERROR: &str = "lsp_server_error";

pub(crate) fn err(code: &str, message: impl Into<String>) -> CoreError {
    CoreError::new(code, &message.into())
}
