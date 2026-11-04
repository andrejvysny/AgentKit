//! Gate for the MCP tests that spawn Bun against the TypeScript SDK
//! fixture. These belong to the mixed-runtime qualification gate: a job
//! that does not provision Bun (the pure-native Windows scope) sets
//! `AGENTKIT_SKIP_TS_INTEROP` and the test returns early with a loud note.
//! Without the opt-out the harness is required: a missing Bun fails the
//! test loudly instead of passing vacuously.

pub(crate) fn ts_interop_skipped(suite: &str) -> bool {
    if std::env::var_os("AGENTKIT_SKIP_TS_INTEROP").is_some() {
        eprintln!(
            "SKIP: {suite} spawns the Bun TypeScript fixture (AGENTKIT_SKIP_TS_INTEROP is set); \
             required in jobs that provision Bun"
        );
        return true;
    }
    false
}
