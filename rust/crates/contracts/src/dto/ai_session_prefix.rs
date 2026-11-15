use crate::{AiToolDefinition, Optional};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;

/// The frozen head of a session's prompt (S1-S4), recorded once and replaced
/// only by a re-freeze. Digests are lowercase SHA-256 hex.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AiSessionPrefix {
    /// Starts at 1; bumped on every re-freeze.
    #[serde(rename = "revision", serialize_with = "crate::serialize_number")]
    pub revision: f64,
    /// S1: the tool definitions as sent.
    #[serde(rename = "toolsDigest")]
    pub tools_digest: String,
    /// S2: profile and host policy text.
    #[serde(rename = "systemDigest")]
    pub system_digest: String,
    /// S3: the project rules block.
    #[serde(rename = "rulesDigest")]
    pub rules_digest: String,
    /// S4: the core memory block.
    #[serde(rename = "memoryCoreDigest")]
    pub memory_core_digest: String,
    /// Model, reasoning and sampling parameters held fixed for the session.
    #[serde(rename = "paramsDigest")]
    pub params_digest: String,
    #[serde(rename = "frozenAtTask")]
    pub frozen_at_task: String,
    #[serde(rename = "cacheKey")]
    pub cache_key: String,
    /// Tool manifest hash (roadmap 1.3); absent until a manifest exists.
    #[serde(
        rename = "toolManifestHash",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub tool_manifest_hash: Optional<String>,
    /// The frozen tool manifest (roadmap 1.3): every tool the session was
    /// frozen with, sorted by name. Later turns send these definitions, not
    /// what the contributors answer now.
    #[serde(
        rename = "toolManifest",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub tool_manifest: Optional<Vec<AiToolDefinition>>,
    /// Tools that joined after the freeze (late contributors), sorted by
    /// name. Listed, never sent; a loader (roadmap 2.3) serves them.
    #[serde(
        rename = "deferredTools",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub deferred_tools: Optional<Vec<AiToolDefinition>>,
    /// Model profile id (native tool use T2.1); absent until profiles exist.
    #[serde(
        rename = "modelProfileId",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub model_profile_id: Optional<String>,
    /// Tool dialect version (native tool use T2.1).
    #[serde(
        rename = "dialectVersion",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub dialect_version: Optional<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AiSessionPrefix, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        revision: crate::required_field(&mut object, "revision")?,
        tools_digest: crate::required_field(&mut object, "toolsDigest")?,
        system_digest: crate::required_field(&mut object, "systemDigest")?,
        rules_digest: crate::required_field(&mut object, "rulesDigest")?,
        memory_core_digest: crate::required_field(&mut object, "memoryCoreDigest")?,
        params_digest: crate::required_field(&mut object, "paramsDigest")?,
        frozen_at_task: crate::required_field(&mut object, "frozenAtTask")?,
        cache_key: crate::required_field(&mut object, "cacheKey")?,
        tool_manifest_hash: crate::optional_field(&mut object, "toolManifestHash")?,
        model_profile_id: crate::optional_field(&mut object, "modelProfileId")?,
        dialect_version: crate::optional_field(&mut object, "dialectVersion")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
        tool_manifest: crate::optional_field(&mut object, "toolManifest")?,
        deferred_tools: crate::optional_field(&mut object, "deferredTools")?,
