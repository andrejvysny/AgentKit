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
//! [`InMemoryMemoryRepository`] and [`InMemoryRetrievalIndex`] are the memory
//! context's repository and index ports held in memory, [`HashEmbedder`] is
//! its deterministic fake embedder; each port has a suite under
//! [`conformance`] (the no-op reranker is `agentkit_memory::NoopReranker`).
//! [`InMemoryOutputStore`] is the core `OutputStore` held in memory (roadmap
//! 2.1, ADR 0034), with its own suite.
//! [`InMemoryWorkspaceRepository`] is the workspaces context's
//! `WorkspaceRepository` held in memory (ADR 0038), with its own suite.
//! [`FakeSandboxBackend`] is its scriptable `SandboxBackend` with scripted
//! faults; the backend suite runs on it.

pub mod cache_provider;
pub mod conformance;
pub mod context_fixtures;
mod mcp_memory;
mod mem_embedder;
mod mem_index;
mod mem_repository;
mod memory;
mod output_memory;
pub mod prefix_harness;
mod sandbox_backend;
pub mod scripted;
mod session_memory;
mod workspace_memory;

pub use mcp_memory::{
    InMemoryMcpConfigFixture, InMemoryMcpConfigRepository, InMemoryMcpConfigRepositoryError,
};
pub use mem_embedder::{HASH_EMBEDDER_DIMS, HashEmbedder};
pub use mem_index::InMemoryRetrievalIndex;
pub use mem_repository::InMemoryMemoryRepository;
pub use memory::{InMemoryRepositoryError, InMemorySessionSettle, InMemoryTaskRepository};
pub use output_memory::{InMemoryOutputStore, ManualClock};
pub use sandbox_backend::{
    CONNECT_REFUSED_EXIT, DISK_FULL_EXIT, FakeSandboxBackend, Program, ProgramEnd, UNSCRIPTED_EXIT,
};
pub use session_memory::{
    CONTEXT_SUMMARIES_NAMESPACE, InMemorySessionFixture, InMemorySessionRepository,
    InMemorySessionRepositoryError, PROMPT_TEMPLATES_NAMESPACE, PROVIDER_CAPABILITIES_NAMESPACE,
    PROVIDERS_NAMESPACE, ReceiptRefusal,
};
pub use workspace_memory::InMemoryWorkspaceRepository;

pub use scripted::{
    SCRIPTED_BACKEND_ID, SCRIPTED_OBSERVED_MODEL, SCRIPTED_VERSION, ScriptedAdapter,
    ScriptedBackend, ScriptedChatProvider, ScriptedChild, ScriptedLocator, ScriptedSupervisor,
    plain_chat_target, scripted_turn_input, supported, unsupported,
};
