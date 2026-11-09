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

mod advisor_lane;
mod authority;
#[cfg(test)]
mod authority_tests;
mod coding_lane;
mod errors;
mod jsbridge;
mod lsp_lane;
mod profiles;
pub use profiles::{list_native_profiles, resolve_native_profile};
mod provider;
mod provider_connection;
mod run;
mod run_budget;
mod run_hooks;
mod run_provider;
mod run_retry;
mod run_steering;
mod subagent_lane;
mod teammate_lane;
pub use run_provider::{
    end_run_provider, is_run_callback_active, push_run_provider_event, reserve_run_provider_request,
};
mod service;
mod store;
mod tool;
mod workspace_binding;

/// `cargo test` links this crate outside a Node process, so the four
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

    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn napi_release_threadsafe_function(
        _tsfn: *mut c_void,
        _mode: i32,
    ) -> i32 {
        0
    }
}

/// The JS wrapper settles exactly one in-flight provider/tool callback with
/// either `{ payload }` (the JSON result) or `{ error }` (a rejection
/// message). Returns false for unknown or already-settled ids: a stale
/// resolve is ignored, never an exception, and never a double-settle.
#[napi(catch_unwind)]
pub fn resolve_call(call_id: String, payload: Option<String>, error: Option<String>) -> bool {
    jsbridge::resolve(&call_id, payload, error)
}

/// Synchronous liveness claim for one routed provider/tool callback: the
/// router asks this BEFORE entering user code. True exactly once, only
/// while Rust still considers the call live (not settled, rejected,
/// cancelled, abandoned by the executor, or drained by close); on false the
/// router must settle nothing and must never run the user function — the
/// host has already given up on this invocation.
#[napi(catch_unwind)]
pub fn claim_call(call_id: String) -> bool {
    jsbridge::claim(&call_id)
}

/// Load-time identity probe: proves the addon, its runtime, and the error
/// contract are all present before any session call is attempted.
#[napi(catch_unwind)]
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

/// The in-flight callback must still own the host's request authority.
#[napi]
pub async fn assert_provider_authority(call_id: String) -> napi::Result<()> {
    jsbridge::check_provider_authority(&call_id)
        .await
        .map_err(|error| {
            errors::stable(
                "callback_rejected",
                "Provider request authority is no longer valid.",
                Some(serde_json::json!({"inactive":error.code == "callback_not_active"})),
            )
        })
}
