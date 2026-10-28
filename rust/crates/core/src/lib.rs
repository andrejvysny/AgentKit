//! Provider-neutral asynchronous chat execution. Secrets and connection policy belong to hosts.
mod budget;
mod dispatch;
mod envelope;
pub mod permission;
pub mod providers;
mod registry;
mod run;
mod runtime;
pub mod sse;
mod tool_execution;
mod types;

pub use budget::{BudgetCheckpoint, BudgetLimits, BudgetSnapshot, ExecutionBudget};
pub use dispatch::{DispatchFence, DispatchGuard};
pub use registry::{RegistryError, ToolRegistry};
pub use run::{run_chat, RunHandle, RunOptions, RunResult, Terminal};
pub use runtime::{Runtime, SystemRuntime};
pub use tokio_util::sync::CancellationToken;
pub use types::*;

/// Model-facing envelope for a tool result. Hosts reconciling durable history
/// must render a replayed recorded result exactly as live dispatch did.
pub fn tool_model_envelope(result: &ToolResult, limits: &ToolLimits) -> String {
    envelope::stringify(&envelope::build(result, limits))
}
