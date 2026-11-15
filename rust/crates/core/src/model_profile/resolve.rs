//! Profile resolution at session start (T2.1).
//!
//! Limits and capabilities come from, in order: the user's setting for the
//! model, the proxy's model information, the server's model list, the probe;
//! a model none of them knows keeps its profile defaults. The profile itself
//! is matched by provider and model id.

use super::builtin::{
    ANTHROPIC_PROFILE_ID, GENERIC_PROFILE_ID, OPENAI_PROFILE_ID, QWEN_PROFILE_ID, builtin_profile,
};
use super::validate::{ProfileError, validate_profile, validate_profile_json};
use agentkit_contracts::{AiModelLimitSource, AiModelToolProfile};
use serde_json::Value;

/// What one source says about a model's limits. A source without a context
/// window says nothing usable and is skipped.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LimitFacts {
    pub context_window: Option<u64>,
    pub max_output: Option<u64>,
}

/// Limit facts per source. The user's own profile record, when given, is the
/// user's setting too and ranks directly after `user`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ModelFacts {
    pub user: Option<LimitFacts>,
    pub proxy: Option<LimitFacts>,
    pub server: Option<LimitFacts>,
    pub probe: Option<LimitFacts>,
}

pub struct ProfileQuery<'a> {
    pub provider: &'a str,
    pub model: &'a str,
    pub facts: ModelFacts,
    /// A full profile record the user configured for this model; it replaces
    /// the built-in match and is validated like any configuration.
    pub user_profile: Option<&'a Value>,
}

/// `*` matches any run of characters; everything else matches case-blind.
fn glob(pattern: &str, text: &str) -> bool {
    let pattern = pattern.to_lowercase();
    let text = text.to_lowercase();
    let mut parts = pattern.split('*');
    let first = parts.next().unwrap_or_default();
    let Some(mut rest) = text.strip_prefix(first) else {
        return false;
    };
    let parts: Vec<&str> = parts.collect();
    let Some((last, middle)) = parts.split_last() else {
        return rest.is_empty();
    };
    for part in middle {
        match rest.find(part) {
            Some(at) => rest = &rest[at + part.len()..],
            None => return false,
        }
    }
    rest.ends_with(last)
}

/// First rule whose provider and model patterns both match wins.
const MATCH_RULES: [(&str, &str, &str); 3] = [
    ("*", "*qwen3.8*27b*", QWEN_PROFILE_ID),
    ("anthropic*", "*", ANTHROPIC_PROFILE_ID),
    ("openai", "*", OPENAI_PROFILE_ID),
];

fn matched_id(provider: &str, model: &str) -> &'static str {
    MATCH_RULES
        .iter()
        .find(|(provider_rule, model_rule, _)| {
            glob(provider_rule, provider) && glob(model_rule, model)
        })
        .map_or(GENERIC_PROFILE_ID, |(_, _, id)| *id)
}

fn apply(profile: &mut AiModelToolProfile, facts: LimitFacts, source: AiModelLimitSource) {
    let Some(context_window) = facts.context_window else {
        return;
    };
    let context_window = context_window as f64;
    let max_output = facts
        .max_output
        .map_or(profile.limits.max_output, |value| value as f64);
    profile.limits.context_window = context_window;
    profile.limits.max_output = max_output.min(context_window);
    profile.limits.source = source;
}

/// Resolve the profile for one session. The result is a complete, validated
/// record; the caller freezes it.
pub fn resolve_profile(query: &ProfileQuery<'_>) -> Result<AiModelToolProfile, ProfileError> {
    let mut profile = match query.user_profile {
        Some(record) => validate_profile_json(record)?,
        None => builtin_profile(matched_id(query.provider, query.model))
            .expect("every match rule names a built-in profile"),
    };
    if let Some((facts, source)) = best_facts(&query.facts, query.user_profile.is_some()) {
        apply(&mut profile, facts, source);
    }
    validate_profile(&profile)?;
    Ok(profile)
}

/// The highest-ranked usable source. A user-configured profile record is
/// itself the user setting, so it outranks the looked-up sources.
fn best_facts(facts: &ModelFacts, user_record: bool) -> Option<(LimitFacts, AiModelLimitSource)> {
    let usable = |fact: Option<LimitFacts>, source| {
        fact.filter(|fact| fact.context_window.is_some())
            .map(|fact| (fact, source))
    };
    usable(facts.user, AiModelLimitSource::User).or_else(|| {
        if user_record {
            return None;
        }
        usable(facts.proxy, AiModelLimitSource::Proxy)
            .or_else(|| usable(facts.server, AiModelLimitSource::Server))
            .or_else(|| usable(facts.probe, AiModelLimitSource::Probe))
    })
}
