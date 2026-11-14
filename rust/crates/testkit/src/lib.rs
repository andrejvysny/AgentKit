//! Test fakes and conformance suites for AgentKit ports (ADR 0021, R10).
//!
//! Dev-only: production crates must never depend on this crate; check-arch
//! rejects a non-dev edge (R3). Each repository trait has one conformance
//! suite under [`conformance`], run against every implementation: the SQLite
//! store in `agentkit-store`'s tests and the in-memory fake here.
//! [`InMemoryTaskRepository`] is the engine's `TaskRepository` held in
//! memory, for engine tests that need no database. [`scripted`] holds the
//! scripted native ports (backend, supervisor, child, frame adapter,
//! locator) and the scripted chat provider, so exactly one copy of them
//! exists for the whole workspace. [`InMemorySessionRepository`] is the
//! sessions context's `SessionRepository` held in memory; both it and
//! [`InMemoryTaskRepository`] run one conformance suite each.
//! [`InMemoryMcpConfigRepository`] is the MCP configuration context's
//! `McpConfigRepository` held in memory, with its own suite.

pub mod conformance;
mod mcp_memory;
mod memory;
pub mod scripted;
mod session_memory;

pub use mcp_memory::{
    InMemoryMcpConfigFixture, InMemoryMcpConfigRepository, InMemoryMcpConfigRepositoryError,
};
pub use memory::{InMemoryRepositoryError, InMemorySessionSettle, InMemoryTaskRepository};
pub use session_memory::{
    CONTEXT_SUMMARIES_NAMESPACE, InMemorySessionFixture, InMemorySessionRepository,
    InMemorySessionRepositoryError, PROMPT_TEMPLATES_NAMESPACE, PROVIDER_CAPABILITIES_NAMESPACE,
    PROVIDERS_NAMESPACE, ReceiptRefusal,
};

pub use scripted::{
    SCRIPTED_BACKEND_ID, SCRIPTED_OBSERVED_MODEL, SCRIPTED_VERSION, ScriptedAdapter,
    ScriptedBackend, ScriptedChatProvider, ScriptedChild, ScriptedLocator, ScriptedSupervisor,
    plain_chat_target, scripted_turn_input, supported, unsupported,
};
