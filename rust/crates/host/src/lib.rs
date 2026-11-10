//! Durable native execution. Subscribers observe persisted events; dropping a
//! subscriber does not cancel the task that produced them.

mod budget;
mod chat;
mod context_window;
mod continuation;
mod emulated_tool_call;
mod governance;
mod mcp_configuration;
mod ports;
mod profiles;
mod project_instructions;
mod proposals;
mod provider_registry;
mod run_content;
mod run_profiles;
mod session;

pub use budget::{
    RoleTaskOverrides, parse_budget_limits, restore_execution_budget, task_overrides,
};
pub use chat::*;
pub use emulated_tool_call::{EMULATED_TOOL_CALL_MESSAGE, looks_like_emulated_tool_call};
pub use governance::*;
pub use mcp_configuration::*;
pub use ports::*;
pub use profiles::*;
pub use project_instructions::*;
pub use proposals::*;
pub use provider_registry::ProviderRegistry;
pub use run_content::*;
pub use run_profiles::*;
pub use session::*;

use agentkit_store::{SqliteStore, StoreError};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub use agentkit_engine::{
    AdmissionGate, ExecutionErrorClassification, ExecutionErrorKind, ExecutionFailure,
    ObserverError, RetryBackoff, RetrySafety, RunUpdate, Runtime, SessionFlushPlanner,
    SharedRuntime, SystemRuntime, TaskExecutor, TaskSettledObserver, VerificationHook,
    VerificationReport, WakeHandle, classify_execution_error,
};

/// The engine's host bound to the SQLite store, the only repository the
/// product ships.
pub type AgentHost = agentkit_engine::AgentHost<SqliteStore>;
pub type ExecutionContext = agentkit_engine::ExecutionContext<SqliteStore>;
pub type HostError = agentkit_engine::EngineError<StoreError>;
pub type HostResult<T> = Result<T, HostError>;
pub type HostOptions = agentkit_engine::HostOptions<SqliteStore>;
pub type RunHandle = agentkit_engine::RunHandle<SqliteStore>;
pub type VerificationInput = agentkit_engine::VerificationInput<SqliteStore>;
pub type WorkerHandle = agentkit_engine::WorkerHandle<SqliteStore>;
