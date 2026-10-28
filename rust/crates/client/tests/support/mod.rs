//! Shared gate for the mixed Rust/TypeScript runtime suites.
//!
//! These tests spawn Bun against the actual TypeScript service and therefore
//! belong to the mixed-runtime qualification gate, not the pure-native suite.
//! A job without Bun or JS dependencies (the Windows pure-native job) sets
//! `AGENTKIT_SKIP_TS_INTEROP` and the test returns early with a loud note.
//! Without that opt-out the harness is required: a missing Bun fails the test
//! loudly instead of passing vacuously. The `native` job must never set the
//! variable; `scripts/tests/workflows.test.ts` pins that policy.

/// Returns true when the mixed-runtime suite is explicitly opted out.
pub fn ts_interop_skipped() -> bool {
    if std::env::var_os("AGENTKIT_SKIP_TS_INTEROP").is_some() {
        eprintln!(
            "SKIP: mixed Rust/TypeScript client interop (AGENTKIT_SKIP_TS_INTEROP is set); \
             these tests are required in the `native` job"
        );
        return true;
    }
    false
}
