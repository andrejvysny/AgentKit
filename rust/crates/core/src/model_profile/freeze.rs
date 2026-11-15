//! Freezing a profile with its session (TU-15).

use super::validate::ProfileError;
use agentkit_contracts::{AiModelToolProfile, AiSessionPrefix, Optional};
use agentkit_kernel::canonical_sha256;

/// SHA-256 of the profile's canonical JSON.
pub fn model_profile_digest(profile: &AiModelToolProfile) -> String {
    canonical_sha256(&serde_json::to_value(profile).expect("model profile serializes"))
}

/// Record the profile's id, dialect version and digest in the prefix.
/// Returns whether the prefix changed.
pub fn stamp_model_profile(prefix: &mut AiSessionPrefix, profile: &AiModelToolProfile) -> bool {
    let id = Optional::Present(profile.id.clone());
    let version = Optional::Present(profile.dialect.version.clone());
    let digest = Optional::Present(model_profile_digest(profile));
    let changed = prefix.model_profile_id != id
        || prefix.dialect_version != version
        || prefix.model_profile_digest != digest;
    prefix.model_profile_id = id;
    prefix.dialect_version = version;
    prefix.model_profile_digest = digest;
    changed
}

/// The session's profile: the one already frozen with it, or, on the first
/// call, the freshly resolved one. A change to a built-in profile or to
/// settings therefore never reaches a running session. The flag says the
/// returned profile is new and the caller must persist it.
pub fn frozen_profile(
    frozen: Option<AiModelToolProfile>,
    resolve: impl FnOnce() -> Result<AiModelToolProfile, ProfileError>,
) -> Result<(AiModelToolProfile, bool), ProfileError> {
    match frozen {
        Some(profile) => Ok((profile, false)),
        None => Ok((resolve()?, true)),
    }
}
