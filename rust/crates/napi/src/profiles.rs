//! Pure native profile queries need neither a store nor a provider.
use napi::bindgen_prelude::*;
use serde_json::Value;

fn registry(overrides: Option<&str>) -> Result<agentkit_sessions::ProfileRegistry> {
    let values: Vec<Value> = overrides
        .map(serde_json::from_str)
        .transpose()
        .map_err(|_| crate::errors::invalid("Profile overrides must be a JSON array."))?
        .unwrap_or_default();
    agentkit_sessions::ProfileRegistry::new(&values)
        .map_err(|error| crate::errors::invalid(error.to_string()))
}
fn resolved(profile: agentkit_sessions::AgentProfile) -> Result<Value> {
    let digest = profile.digest();
    let mut value = serde_json::to_value(profile)
        .map_err(|_| crate::errors::internal("Profile serialization failed."))?;
    value["digest"] = digest.into();
    Ok(value)
}
#[napi(catch_unwind)]
pub fn list_native_profiles(overrides_json: Option<String>) -> Result<String> {
    let profiles = registry(overrides_json.as_deref())?
        .list()
        .into_iter()
        .map(resolved)
        .collect::<Result<Vec<_>>>()?;
    serde_json::to_string(&profiles)
        .map_err(|_| crate::errors::internal("Profile serialization failed."))
}
#[napi(catch_unwind)]
pub fn resolve_native_profile(
    profile_id: String,
    overrides_json: Option<String>,
) -> Result<String> {
    let profile = registry(overrides_json.as_deref())?
        .resolve(&profile_id)
        .map_err(|error| crate::errors::invalid(error.to_string()))?;
    serde_json::to_string(&resolved(profile)?)
        .map_err(|_| crate::errors::internal("Profile serialization failed."))
}
