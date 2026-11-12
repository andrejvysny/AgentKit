//! The native lane: the ports `AgentRuntimeBuilder::native_lane` accepts so
//! the turn executor can run admitted native targets through
//! `agentkit-native-exec` without the runtime ever naming a driver crate
//! (ADR 0025 "the host never depends on any driver crate"; ADR 0026).
//!
//! The consumer wires the concrete backend, supervisor and locator (the SDK
//! facade's `claude` feature passes `agentkit-claude`'s); the optional
//! broker factory supplies the governed per-attempt tool broker.

use std::sync::Arc;

use agentkit_native::{ExecutableLocator, NativeBackend, ProcessSupervisor};
use agentkit_native_exec::{AttemptBrokerFactory, NativeExecutorOptions};

/// Everything the native branch of the turn executor needs, provided by the
/// consumer at composition time.
pub struct NativeLane {
    pub backend: Arc<dyn NativeBackend>,
    pub supervisor: Arc<dyn ProcessSupervisor>,
    pub locator: Arc<dyn ExecutableLocator>,
    pub broker_factory: Option<Arc<dyn AttemptBrokerFactory>>,
    pub options: NativeExecutorOptions,
}
