//! Generic durable task execution: the worker, claim/lease/settle/recovery,
//! retry classification and run subscription. Knows nothing about chats,
//! sessions or profiles; those contexts plug in through the ports here.

mod child_wait;
mod context;
mod dispatch;
mod error;
mod model;
mod ports;
mod repository;
mod retry;
mod runner;
mod subscription;

pub use child_wait::{CHILD_WAIT_ID_PREFIX, CHILD_WAIT_KIND, claims_child_wait, is_child_wait};
pub use context::ExecutionContext;
pub use error::{ContextClassification, EngineError, EngineResult, NeverFault};
pub use model::{
    AttemptRecord, ClaimInput, ClaimedTask, EventRecord, Lease, LeaseFence, MessageInput,
    PinnedRunProfile, RecoveryMode, RetryTask, SettleTask, SubmitTask, TaskBinding, TaskRecord,
    TaskStatus, TaskTransitionPatch,
};
pub use ports::{
    ObserverError, Runtime, SharedRuntime, SystemRuntime, TaskExecutor, TaskSettledObserver,
    VerificationHook, VerificationInput, VerificationReport,
};
pub use repository::{RepositoryFault, SessionSettlement, TaskInputReason, TaskRepository};
pub use retry::{
    ExecutionErrorClassification, ExecutionErrorKind, ExecutionFailure, RetryBackoff, RetrySafety,
    classify_execution_error,
};
pub use runner::{
    AdmissionGate, AgentHost, HostOptions, SessionFlushPlanner, WakeHandle, WorkerHandle,
};
pub use subscription::{RunHandle, RunUpdate};
