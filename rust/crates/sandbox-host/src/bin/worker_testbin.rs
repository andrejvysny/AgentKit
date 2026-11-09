//! Faithful worker daemon binary for the host-bridge L1 tests.
//!
//! Includes the worker crate's own CLI entry point (normalized by `build.rs`
//! for `include!` legality) under a unique artifact name, so
//! `cargo test -p agentkit-sandbox-host` always spawns the *current* worker
//! binary — no stale-binary risk, no reimplementation of worker semantics.

mod worker_daemon {
    include!(concat!(env!("OUT_DIR"), "/worker_cli.rs"));

    pub fn entry() {
        main();
    }
}

fn main() {
    worker_daemon::entry();
}
