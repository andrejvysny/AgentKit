//! Test fakes and conformance suites for AgentKit ports (ADR 0021, R10).
//!
//! Dev-only: production crates must never depend on this crate; check-arch
//! rejects a non-dev edge (R3). Each repository trait has one conformance
//! suite under [`conformance`], run against every implementation: the SQLite
//! store in `agentkit-store`'s tests and the in-memory fake here.
//! [`InMemoryTaskRepository`] is the engine's `TaskRepository` held in
//! memory, for engine tests that need no database.

pub mod conformance;
mod memory;

pub use memory::{InMemoryRepositoryError, InMemorySessionSettle, InMemoryTaskRepository};
