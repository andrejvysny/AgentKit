// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AiProviderModelPricingPriceSource {
    #[serde(rename = "host")]
    Host,
    #[serde(rename = "provider")]
    Provider,
    #[serde(rename = "preset")]
    Preset,
}
crate::impl_json_via_serde!(AiProviderModelPricingPriceSource);

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AiProviderModelPricing {
    #[serde(rename = "currency")]
    pub currency: String,
    #[serde(
        rename = "inputPerMillionTokens",
        default,
        skip_serializing_if = "Optional::is_absent",
        serialize_with = "crate::serialize_optional_number"
    )]
    pub input_per_million_tokens: Optional<f64>,
    #[serde(
        rename = "outputPerMillionTokens",
        default,
        skip_serializing_if = "Optional::is_absent",
        serialize_with = "crate::serialize_optional_number"
    )]
    pub output_per_million_tokens: Optional<f64>,
    #[serde(
        rename = "cachedInputPerMillionTokens",
        default,
        skip_serializing_if = "Optional::is_absent",
        serialize_with = "crate::serialize_optional_number"
    )]
    pub cached_input_per_million_tokens: Optional<f64>,
    #[serde(
        rename = "reasoningTokensBilledAsOutput",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub reasoning_tokens_billed_as_output: Optional<bool>,
    #[serde(rename = "priceSource")]
    pub price_source: AiProviderModelPricingPriceSource,
    #[serde(
        rename = "effectiveDate",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub effective_date: Optional<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AiProviderModelPricing, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        currency: crate::required_field(&mut object, "currency")?,
        input_per_million_tokens: crate::optional_field(&mut object, "inputPerMillionTokens")?,
        output_per_million_tokens: crate::optional_field(&mut object, "outputPerMillionTokens")?,
        cached_input_per_million_tokens: crate::optional_field(
            &mut object,
            "cachedInputPerMillionTokens",
        )?,
        reasoning_tokens_billed_as_output: crate::optional_field(
            &mut object,
            "reasoningTokensBilledAsOutput",
        )?,
        price_source: crate::required_field(&mut object, "priceSource")?,
        effective_date: crate::optional_field(&mut object, "effectiveDate")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AiProviderModel {
    #[serde(rename = "providerId")]
    pub provider_id: AiProviderKind,
    #[serde(rename = "modelId")]
    pub model_id: AiProviderKind,
    #[serde(rename = "displayName")]
    pub display_name: Nullable<AiProviderKind>,
    #[serde(
        rename = "contextWindowTokens",
        default,
        skip_serializing_if = "Optional::is_absent",
        serialize_with = "crate::serialize_optional_number"
    )]
    pub context_window_tokens: Optional<f64>,
    #[serde(
        rename = "supportsToolCalling",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub supports_tool_calling: Optional<bool>,
    #[serde(
        rename = "supportsStreaming",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub supports_streaming: Optional<bool>,
    #[serde(
        rename = "supportsJsonMode",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub supports_json_mode: Optional<bool>,
    #[serde(
        rename = "supportsVision",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub supports_vision: Optional<bool>,
    #[serde(
        rename = "supportsAudio",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub supports_audio: Optional<bool>,
    #[serde(
        rename = "supportsReasoning",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub supports_reasoning: Optional<bool>,
    #[serde(
        rename = "reasoningEfforts",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub reasoning_efforts: Optional<Vec<AiProviderKind>>,
    #[serde(
        rename = "maxOutputTokens",
        default,
        skip_serializing_if = "Optional::is_absent",
        serialize_with = "crate::serialize_optional_number"
    )]
    pub max_output_tokens: Optional<f64>,
    #[serde(
        rename = "pricing",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub pricing: Optional<AiProviderModelPricing>,
    #[serde(
        rename = "metadataSource",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub metadata_source: Optional<AiProviderModelPricingPriceSource>,
    #[serde(
        rename = "metadataFetchedAt",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub metadata_fetched_at: Optional<AiProviderKind>,
    #[serde(rename = "fetchedAt")]
    pub fetched_at: AiProviderKind,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AiProviderModel, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        provider_id: crate::required_field(&mut object, "providerId")?,
        model_id: crate::required_field(&mut object, "modelId")?,
        display_name: crate::required_field(&mut object, "displayName")?,
        context_window_tokens: crate::optional_field(&mut object, "contextWindowTokens")?,
        supports_tool_calling: crate::optional_field(&mut object, "supportsToolCalling")?,
        supports_streaming: crate::optional_field(&mut object, "supportsStreaming")?,
        supports_json_mode: crate::optional_field(&mut object, "supportsJsonMode")?,
        supports_vision: crate::optional_field(&mut object, "supportsVision")?,
        supports_audio: crate::optional_field(&mut object, "supportsAudio")?,
        supports_reasoning: crate::optional_field(&mut object, "supportsReasoning")?,
        reasoning_efforts: crate::optional_field(&mut object, "reasoningEfforts")?,
        max_output_tokens: crate::optional_field(&mut object, "maxOutputTokens")?,
        pricing: crate::optional_field(&mut object, "pricing")?,
        metadata_source: crate::optional_field(&mut object, "metadataSource")?,
        metadata_fetched_at: crate::optional_field(&mut object, "metadataFetchedAt")?,
        fetched_at: crate::required_field(&mut object, "fetchedAt")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
