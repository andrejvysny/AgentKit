//! Thin Node-API binding over the native AgentKit runtime.
//!
//! Nothing here re-implements session, storage, governance or provider
//! logic: the binding moves JSON across the boundary, enforces the lifetime
//! and error contract documented in
//! `docs/rust/native-binding-milestone.md`, and routes provider/tool calls
//! to exactly one JS callback per in-flight request. Every error that
//! reaches JavaScript carries a code from `error-codes.json`.

use napi::bindgen_prelude::*;

#[macro_use]
extern crate napi_derive;

mod authority;
#[cfg(test)]
mod authority_tests;
mod errors;
mod jsbridge;
mod provider;
mod service;
mod store;
mod tool;

/// `cargo test` links this crate outside a Node process, so the three
/// libnode symbols the napi rlib genuinely references are missing. These
/// stubs exist only in the test binary; they are never reached by the unit
/// tests below (which exercise pure payload logic), and shipping builds
/// link against the real Node/Bun host.
#[cfg(test)]
mod napi_test_stubs {
    use std::os::raw::c_void;

    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn napi_delete_reference(_env: *mut c_void, _ref: *mut c_void) -> i32 {
        0
    }

    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn napi_reference_unref(
        _env: *mut c_void,
        _ref: *mut c_void,
        _count: *mut i32,
    ) -> i32 {
        0
    }

    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn napi_call_threadsafe_function(
        _tsfn: *mut c_void,
        _data: *mut c_void,
        _mode: i32,
    ) -> i32 {
        0
    }
}

/// The JS wrapper settles exactly one in-flight provider/tool callback with
/// either `{ payload }` (the JSON result) or `{ error }` (a rejection
/// message). Returns false for unknown or already-settled ids: a stale
/// resolve is ignored, never an exception, and never a double-settle.
#[napi]
pub fn resolve_call(call_id: String, payload: Option<String>, error: Option<String>) -> bool {
    jsbridge::resolve(&call_id, payload, error)
}

/// Load-time identity probe: proves the addon, its runtime, and the error
/// contract are all present before any session call is attempted.
#[napi]
pub fn binding_smoke() -> Result<String> {
    // Validate the embedded contract at load, not on first error.
    let codes = errors::contract_codes();
    if codes.is_empty() || !codes.iter().any(|code| code == "internal_error") {
        return Err(errors::internal(
            "the embedded error-code contract is malformed",
        ));
    }
    Ok("agentkit-native".to_owned())
}
