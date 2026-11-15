//! Configuration-time validation of a profile record (TU-16, TU-19).

use agentkit_contracts::{
    AiModelLimitSource, AiModelToolProfile, JsonDeserialize, validate_schema,
};
use serde_json::Value;

/// The extra request fields a profile may carry. The list is closed: the
/// schema, this check and the typed record all enforce it.
pub const BODY_EXTRA_ALLOW_LIST: [&str; 2] = ["chat_template_kwargs", "parallel_tool_calls"];

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum ProfileError {
    #[error("body_extras field {0:?} is not in the allow-list")]
    BodyExtraNotAllowed(String),
    #[error("model profile is invalid: {0}")]
    Invalid(String),
    #[error("profile {profile} is qualified, so its limits cannot be estimated")]
    EstimatedLimitForQualified { profile: String },
}

fn invalid(message: impl Into<String>) -> ProfileError {
    ProfileError::Invalid(message.into())
}

/// Decode and validate a profile record received as configuration. The
/// allow-list is checked first so the refusal names the field.
pub fn validate_profile_json(value: &Value) -> Result<AiModelToolProfile, ProfileError> {
    if let Some(extras) = value.get("bodyExtras").and_then(Value::as_object)
        && let Some(field) = extras
            .keys()
            .find(|key| !BODY_EXTRA_ALLOW_LIST.contains(&key.as_str()))
    {
        return Err(ProfileError::BodyExtraNotAllowed(field.clone()));
    }
    validate_schema("AiModelToolProfileSchema", value).map_err(|e| invalid(e.to_string()))?;
    let profile =
        AiModelToolProfile::from_json(value.clone()).map_err(|e| invalid(e.to_string()))?;
    validate_profile(&profile)?;
    Ok(profile)
}

/// The rules a decoded record must also satisfy.
pub fn validate_profile(profile: &AiModelToolProfile) -> Result<(), ProfileError> {
    if profile.direct_tools.len() as f64 > profile.max_visible_tools {
        return Err(invalid("directTools exceeds maxVisibleTools"));
    }
    let limits = &profile.limits;
    if limits.context_window < 1.0 || limits.max_output < 1.0 {
        return Err(invalid("limits must be positive"));
    }
    if limits.max_output > limits.context_window {
        return Err(invalid("maxOutput exceeds contextWindow"));
    }
    if !profile.qualified_toolsets.is_empty() && limits.source == AiModelLimitSource::Estimated {
        return Err(ProfileError::EstimatedLimitForQualified {
            profile: profile.id.clone(),
        });
    }
    for (name, set) in [
        ("thinking", &profile.sampling.thinking),
        ("nonThinking", &profile.sampling.non_thinking),
    ] {
        let unit = |value: Option<&f64>| value.is_none_or(|v| (0.0..=1.0).contains(v));
        if !unit(set.top_p.as_ref()) || !unit(set.min_p.as_ref()) {
            return Err(invalid(format!(
                "sampling.{name} topP/minP must be in 0..=1"
            )));
        }
        if set.temperature.as_ref().is_some_and(|t| *t < 0.0) {
            return Err(invalid(format!("sampling.{name}.temperature must be >= 0")));
        }
    }
    let map = &profile.reasoning.effort_map;
    for (level, entry) in [
        ("off", &map.off),
        ("minimal", &map.minimal),
        ("low", &map.low),
        ("medium", &map.medium),
        ("high", &map.high),
    ] {
        if let Some(entry) = entry.as_ref()
            && !entry.thinking
            && !entry.effort.is_absent()
        {
            return Err(invalid(format!(
                "effortMap.{level} turns thinking off and cannot carry an effort word"
            )));
        }
    }
    Ok(())
}
