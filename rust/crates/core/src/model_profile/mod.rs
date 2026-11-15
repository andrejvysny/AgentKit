//! Model tool profiles (native tool use T2.1, ADR 0030).
//!
//! A [`AiModelToolProfile`] says how one model wants to be offered tools:
//! dialect, visible-tool bound, load mode, tool-choice policy, sampling,
//! reasoning mapping, extra request fields, message rules, limits and
//! recovery. It is resolved when a session starts and frozen with the
//! session; the prefix records its id, dialect version and digest. Applying
//! the values to a request is the request builder's job (T2.2), not this
//! module's.

mod builtin;
mod freeze;
mod messages;
mod request;
mod resolve;
mod salvage;
mod validate;

pub use agentkit_contracts::AiModelToolProfile;
pub use builtin::{
    ANTHROPIC_PROFILE_ID, GENERIC_PROFILE_ID, OPENAI_PROFILE_ID, QWEN_PROFILE_ID, builtin_profile,
};
pub use freeze::{frozen_profile, model_profile_digest, stamp_model_profile};
pub use messages::{
    TOOL_RESPONSE_CLOSE, TOOL_RESPONSE_OPEN, apply_message_rules, escape_tool_response_tags,
};
pub use request::{
    ProfileNote, QWEN_EFFORT_TEMPLATE_KEY, REASONING_HISTORY_FIELD, REASONING_METADATA_KEY,
    RequestSettings, THINKING_TEMPLATE_KEY, ToolChoice, policy_tool_choice,
    resolve_request_settings, wire_tool_choice,
};
pub use resolve::{LimitFacts, ModelFacts, ProfileQuery, resolve_profile};
pub use salvage::{Salvage, SalvagedCall, nudge_after_announced_call, salvage_tool_calls};
pub use validate::{BODY_EXTRA_ALLOW_LIST, ProfileError, validate_profile, validate_profile_json};
