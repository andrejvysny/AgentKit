use crate::Optional;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

macro_rules! string_enum {
    ($(#[$doc:meta])* $name:ident { $($variant:ident = $wire:literal),+ $(,)? }) => {
        $(#[$doc])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
        pub enum $name {
            $(#[serde(rename = $wire)] $variant),+
        }
        crate::impl_json_via_serde!($name);
    };
}

string_enum!(AiModelFamily {
    Anthropic = "anthropic",
    Openai = "openai",
    Qwen = "qwen",
    Generic = "generic",
});
string_enum!(
    /// How a loaded toolset becomes callable.
    AiModelLoadMode {
        Indirect = "indirect",
        Promote = "promote",
    }
);
string_enum!(
    /// What `tool_choice` values may be sent.
    AiModelToolChoicePolicy {
        AutoOnly = "auto_only",
        AutoRequiredNamed = "auto_required_named",
    }
);
string_enum!(
    /// Which request field carries the reasoning effort.
    AiModelEffortChannel {
        EffortField = "effort_field",
        TemplateOption = "template_option",
        None = "none",
    }
);
string_enum!(AiModelImagePosition {
    User = "user",
    ToolResult = "tool_result",
});
string_enum!(
    /// Where a limit came from; `estimated` is never allowed for a qualified model.
    AiModelLimitSource {
        User = "user",
        Proxy = "proxy",
        Server = "server",
        Probe = "probe",
        Estimated = "estimated",
    }
);

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AiModelDialect {
    #[serde(rename = "id")]
    pub id: String,
    #[serde(rename = "version")]
    pub version: String,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AiModelDialect, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        id: crate::required_field(&mut object, "id")?,
        version: crate::required_field(&mut object, "version")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});

/// One sampling set; absent fields are left to the provider default.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct AiModelSamplingSet {
    #[serde(
        rename = "temperature",
        default,
        skip_serializing_if = "Optional::is_absent",
        serialize_with = "crate::serialize_optional_number"
    )]
    pub temperature: Optional<f64>,
    #[serde(
        rename = "topP",
        default,
        skip_serializing_if = "Optional::is_absent",
        serialize_with = "crate::serialize_optional_number"
    )]
    pub top_p: Optional<f64>,
    #[serde(
        rename = "topK",
        default,
        skip_serializing_if = "Optional::is_absent",
        serialize_with = "crate::serialize_optional_number"
    )]
    pub top_k: Optional<f64>,
    #[serde(
        rename = "minP",
        default,
        skip_serializing_if = "Optional::is_absent",
        serialize_with = "crate::serialize_optional_number"
    )]
    pub min_p: Optional<f64>,
    #[serde(
        rename = "presencePenalty",
        default,
        skip_serializing_if = "Optional::is_absent",
        serialize_with = "crate::serialize_optional_number"
    )]
    pub presence_penalty: Optional<f64>,
    #[serde(
        rename = "repetitionPenalty",
        default,
        skip_serializing_if = "Optional::is_absent",
        serialize_with = "crate::serialize_optional_number"
    )]
    pub repetition_penalty: Optional<f64>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AiModelSamplingSet, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        temperature: crate::optional_field(&mut object, "temperature")?,
        top_p: crate::optional_field(&mut object, "topP")?,
        top_k: crate::optional_field(&mut object, "topK")?,
        min_p: crate::optional_field(&mut object, "minP")?,
        presence_penalty: crate::optional_field(&mut object, "presencePenalty")?,
        repetition_penalty: crate::optional_field(&mut object, "repetitionPenalty")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AiModelSampling {
    #[serde(rename = "thinking")]
    pub thinking: AiModelSamplingSet,
    #[serde(rename = "nonThinking")]
    pub non_thinking: AiModelSamplingSet,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AiModelSampling, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        thinking: crate::required_field(&mut object, "thinking")?,
        non_thinking: crate::required_field(&mut object, "nonThinking")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});

/// What one OneMind effort level means for this model.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AiModelEffortSetting {
    #[serde(rename = "thinking")]
    pub thinking: bool,
    /// The model's own effort word; absent when thinking is off.
    #[serde(
        rename = "effort",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub effort: Optional<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AiModelEffortSetting, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        thinking: crate::required_field(&mut object, "thinking")?,
        effort: crate::optional_field(&mut object, "effort")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});

/// Effort levels the profile maps; an unmapped level is not sent.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AiModelEffortMap {
    #[serde(rename = "off", default, skip_serializing_if = "Optional::is_absent")]
    pub off: Optional<AiModelEffortSetting>,
    #[serde(
        rename = "minimal",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub minimal: Optional<AiModelEffortSetting>,
    #[serde(rename = "low", default, skip_serializing_if = "Optional::is_absent")]
    pub low: Optional<AiModelEffortSetting>,
    #[serde(
        rename = "medium",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub medium: Optional<AiModelEffortSetting>,
    #[serde(rename = "high", default, skip_serializing_if = "Optional::is_absent")]
    pub high: Optional<AiModelEffortSetting>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AiModelEffortMap, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        off: crate::optional_field(&mut object, "off")?,
        minimal: crate::optional_field(&mut object, "minimal")?,
        low: crate::optional_field(&mut object, "low")?,
        medium: crate::optional_field(&mut object, "medium")?,
        high: crate::optional_field(&mut object, "high")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AiModelReasoning {
    #[serde(rename = "effortChannel")]
    pub effort_channel: AiModelEffortChannel,
    #[serde(rename = "effortMap")]
    pub effort_map: AiModelEffortMap,
    /// Whether stored reasoning is sent back in history.
    #[serde(rename = "sendBack")]
    pub send_back: bool,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AiModelReasoning, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        effort_channel: crate::required_field(&mut object, "effortChannel")?,
        effort_map: crate::required_field(&mut object, "effortMap")?,
        send_back: crate::required_field(&mut object, "sendBack")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});

/// Extra request fields. Closed on purpose: a field outside this allow-list
/// is refused at configuration time (TU-16).
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct AiModelBodyExtras {
    #[serde(
        rename = "chat_template_kwargs",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub chat_template_kwargs: Optional<BTreeMap<String, Value>>,
    #[serde(
        rename = "parallel_tool_calls",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub parallel_tool_calls: Optional<bool>,
}
crate::impl_json_deserialize!(AiModelBodyExtras, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        chat_template_kwargs: crate::optional_field(&mut object, "chat_template_kwargs")?,
        parallel_tool_calls: crate::optional_field(&mut object, "parallel_tool_calls")?,
    };
    crate::reject_extra_fields(&object)?;
    Ok(output)
});

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AiModelMessageRules {
    #[serde(rename = "systemFirstOnly")]
    pub system_first_only: bool,
    #[serde(rename = "escapeToolResultTags")]
    pub escape_tool_result_tags: bool,
    #[serde(rename = "imagePositions")]
    pub image_positions: Vec<AiModelImagePosition>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AiModelMessageRules, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        system_first_only: crate::required_field(&mut object, "systemFirstOnly")?,
        escape_tool_result_tags: crate::required_field(&mut object, "escapeToolResultTags")?,
        image_positions: crate::required_field(&mut object, "imagePositions")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AiModelLimits {
    #[serde(rename = "contextWindow", serialize_with = "crate::serialize_number")]
    pub context_window: f64,
    #[serde(rename = "maxOutput", serialize_with = "crate::serialize_number")]
    pub max_output: f64,
    #[serde(rename = "source")]
    pub source: AiModelLimitSource,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AiModelLimits, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        context_window: crate::required_field(&mut object, "contextWindow")?,
        max_output: crate::required_field(&mut object, "maxOutput")?,
        source: crate::required_field(&mut object, "source")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AiModelRecovery {
    /// Parse tool-call text the server left unparsed, once, and record it.
    #[serde(rename = "salvage")]
    pub salvage: bool,
    /// Nudge after an announced call with none emitted.
    #[serde(rename = "nudge")]
    pub nudge: bool,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AiModelRecovery, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        salvage: crate::required_field(&mut object, "salvage")?,
        nudge: crate::required_field(&mut object, "nudge")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
