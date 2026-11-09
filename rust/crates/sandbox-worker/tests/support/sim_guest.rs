//! Test-side guest simulator façade.
//!
//! The simulator itself lives in `guestlink` (the fake backend needs it at
//! runtime too). This module re-exports it so integration tests read as
//! "spawn the sim guest", and hosts a negative-association helper.

pub use agentkit_sandbox_worker::guestlink::{SimBehavior, SimGuest, duplex_pair};

/// Spawn a simulator with a tampered boot-nonce echo: the worker must
/// refuse association and quarantine the generation (§3.3).
#[must_use]
pub fn wrong_nonce_behavior() -> SimBehavior {
    SimBehavior {
        wrong_nonce: true,
        ..SimBehavior::default()
    }
}
