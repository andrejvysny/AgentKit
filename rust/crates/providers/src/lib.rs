//! Concrete LLM provider adapters and their HTTP plumbing, driven through the
//! `agentkit-core` provider port.
mod chat_completions;
mod chat_completions_request;
pub mod conformance;
mod connection_transport;
mod content;
mod native_provider_factory;
mod presets;
pub mod probe;
mod responses;
mod responses_request;
mod responses_state;
mod sse;
mod transport;
mod usage;

pub use chat_completions::ChatCompletionsProvider;
pub use connection_transport::{NativeConnectionTransport, disclosed_categories};
pub use native_provider_factory::{
    Approval, BindError, NativeProviderBinding, NativeProviderFactory, ProviderApprover,
};
pub use presets::{ProviderPreset, preset_by_kind, provider_presets};
pub use responses::ResponsesProvider;
pub use responses_request::{
    RESPONSES_SCHEMA_KEYS, SchemaLane, allowed_schema_keys, build_responses_request,
    build_responses_request_with_warnings, schema_lane_violations,
};
pub use transport::TrustedTransport;
