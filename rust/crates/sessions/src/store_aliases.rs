//! The engine's generic types bound to the SQLite store, the only repository
//! the product ships.

use agentkit_store::{SqliteStore, StoreError};

pub type AgentHost = agentkit_engine::AgentHost<SqliteStore>;
pub type ExecutionContext = agentkit_engine::ExecutionContext<SqliteStore>;
pub type HostError = agentkit_engine::EngineError<StoreError>;
pub type HostResult<T> = Result<T, HostError>;
pub type HostOptions = agentkit_engine::HostOptions<SqliteStore>;
pub type RunHandle = agentkit_engine::RunHandle<SqliteStore>;
pub type VerificationInput = agentkit_engine::VerificationInput<SqliteStore>;
pub type WorkerHandle = agentkit_engine::WorkerHandle<SqliteStore>;
