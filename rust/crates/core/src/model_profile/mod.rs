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
mod resolve;
mod validate;

pub use agentkit_contracts::AiModelToolProfile;
pub use builtin::{
    ANTHROPIC_PROFILE_ID, GENERIC_PROFILE_ID, OPENAI_PROFILE_ID, QWEN_PROFILE_ID, builtin_profile,
};
pub use freeze::{frozen_profile, model_profile_digest, stamp_model_profile};
pub use resolve::{LimitFacts, ModelFacts, ProfileQuery, resolve_profile};
pub use validate::{BODY_EXTRA_ALLOW_LIST, ProfileError, validate_profile, validate_profile_json};
