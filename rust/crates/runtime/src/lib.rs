//! Native runtime composition (layer 5): the one crate that names concrete
//! adapters. [`AgentRuntime::builder`] wires the host, the chat executor,
//! the session service, the four lane adapters, and the worker; bindings
//! call it and keep only their own transport concerns.

mod authority;
mod builder;
mod error;
mod lanes;
mod runtime;
mod session_execution_authority;
mod workspace;

pub use authority::{PlanConfiguration, effective_class};
pub use builder::{AgentRuntimeBuilder, ExternalToolsPolicy, RefusingExecutor};
pub use error::RuntimeError;
pub use lanes::advisor::AdvisorLane;
pub use lanes::coding::CodingLane;
pub use lanes::lsp::LspLane;
pub use lanes::native::NativeLane;
pub use lanes::subagent::SubagentLane;
pub use runtime::AgentRuntime;
pub use session_execution_authority::SessionExecutionAuthority;
