//! Provider-neutral asynchronous chat execution. Secrets and connection policy belong to hosts.
mod budget;
mod dispatch;
mod envelope;
pub mod permission;
mod provider_identity;
mod provider_retry;
mod registry;
mod run;
mod tool_execution;
mod types;

pub use budget::{BudgetCheckpoint, BudgetLimits, BudgetSnapshot, ExecutionBudget};
// The event type names the public `RunHandle`/`RunEventSink` surfaces; hosts
// (and this crate's own tests) need it without a parallel contracts import.
pub use agentkit_contracts::AiRunEvent;
pub use agentkit_kernel::{Runtime, SystemRuntime};
pub use dispatch::{DispatchFence, DispatchGuard};
pub use provider_identity::{AuthKind, ConnectionIdentity};
pub use provider_retry::{RetryHooks, RetryPolicy, RetryProvider};
pub use registry::{RegistryError, ToolRegistry};
pub use run::{RunEventGate, RunHandle, RunOptions, RunResult, Terminal, run_chat};
pub use tokio_util::sync::CancellationToken;
pub use tool_execution::dedupe as dedupe_tool_call_ids;
pub use types::*;

/// Model-facing envelope for a tool result. Hosts reconciling durable history
/// must render a replayed recorded result exactly as live dispatch did.
pub fn tool_model_envelope(result: &ToolResult, limits: &ToolLimits) -> String {
    envelope::stringify(&envelope::build(result, limits))
}
