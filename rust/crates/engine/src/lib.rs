//! Generic durable task execution: the worker, claim/lease/settle/recovery,
//! retry classification and run subscription. Knows nothing about chats,
//! sessions or profiles; those contexts plug in through the ports here.

mod context;
mod dispatch;
mod error;
mod ports;
mod retry;
mod runner;
mod subscription;

pub use context::ExecutionContext;
pub use error::{EngineError, EngineResult};
pub use ports::{
    Runtime, SharedRuntime, SystemRuntime, TaskExecutor, TaskSettledObserver, VerificationHook,
    VerificationInput, VerificationReport,
};
pub use retry::{
    ExecutionErrorClassification, ExecutionErrorKind, ExecutionFailure, RetryBackoff, RetrySafety,
    classify_execution_error,
};
pub use runner::{
    AdmissionGate, AgentHost, HostOptions, SessionFlushPlanner, SettlePlan, WakeHandle,
    WorkerHandle,
};
pub use subscription::{RunHandle, RunUpdate};
