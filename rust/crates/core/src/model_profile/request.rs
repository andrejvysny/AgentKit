//! Applying a frozen profile to request settings (T2.2, TU-16).
//!
//! Pure: a profile plus what the caller asked for in, the top-level body
//! fields and the notes to record out. A transport merges the fields into its
//! request body and reports every note as a warning, so a downgrade or a
//! dropped value is never silent.

use crate::ReasoningEffort;
use agentkit_contracts::{
    AiModelEffortChannel, AiModelEffortSetting, AiModelSamplingSet, AiModelToolChoicePolicy,
    AiModelToolProfile,
};
use serde_json::{Map, Value, json};

/// The chat-template key that carries the model's effort word on the
/// template-option channel (Qwen family).
///
/// Unverified; set by probe P6. The spec records only that the template
/// accepts the words low, medium and xhigh, not the key it reads them from.
pub const QWEN_EFFORT_TEMPLATE_KEY: &str = "reasoning_effort";
/// The chat-template key that switches thinking off.
pub const THINKING_TEMPLATE_KEY: &str = "enable_thinking";
/// The assistant-message field vLLM reads earlier reasoning from (probe P5
/// confirms the name per endpoint).
pub const REASONING_HISTORY_FIELD: &str = "reasoning_content";
/// Where a stored assistant message keeps its reasoning text.
pub const REASONING_METADATA_KEY: &str = "reasoningContent";

/// What a caller wants `tool_choice` to be. The profile decides what may be
/// sent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ToolChoice {
    #[default]
    Auto,
    Required,
    Named(String),
}

/// Something a profile changed or refused, to be shown as a warning.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileNote {
    pub code: &'static str,
    pub message: String,
}

impl ProfileNote {
    pub(crate) fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// The profile's answer for one request.
#[derive(Clone, Debug, PartialEq)]
pub struct RequestSettings {
    /// Whether this request runs with thinking on; it picks the sampling set.
    pub thinking: bool,
    /// Top-level body fields: sampling, `chat_template_kwargs`,
    /// `parallel_tool_calls`, and `reasoning_effort` on the effort-field
    /// channel.
    pub fields: Map<String, Value>,
    /// The `tool_choice` value to send; `None` when the request has no tools.
    pub tool_choice: Option<Value>,
    pub notes: Vec<ProfileNote>,
}

/// A whole number is sent as an integer (`top_k: 20`), anything else as a
/// float.
fn number(value: f64) -> Value {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        json!(value as i64)
    } else {
        json!(value)
    }
}

fn sampling_fields(set: &AiModelSamplingSet, fields: &mut Map<String, Value>) {
    for (name, value, whole) in [
        ("temperature", &set.temperature, false),
        ("top_p", &set.top_p, false),
        ("top_k", &set.top_k, true),
        ("min_p", &set.min_p, false),
        ("presence_penalty", &set.presence_penalty, false),
        ("repetition_penalty", &set.repetition_penalty, false),
    ] {
        if let Some(value) = value.as_ref() {
            fields.insert(
                name.into(),
                if whole { number(*value) } else { json!(*value) },
            );
        }
    }
}

fn level_name(effort: ReasoningEffort) -> &'static str {
    match effort {
        ReasoningEffort::Minimal => "minimal",
        ReasoningEffort::Low => "low",
        ReasoningEffort::Medium => "medium",
        ReasoningEffort::High => "high",
    }
}

fn mapped(profile: &AiModelToolProfile, effort: ReasoningEffort) -> Option<&AiModelEffortSetting> {
    let map = &profile.reasoning.effort_map;
    match effort {
        ReasoningEffort::Minimal => map.minimal.as_ref(),
        ReasoningEffort::Low => map.low.as_ref(),
        ReasoningEffort::Medium => map.medium.as_ref(),
        ReasoningEffort::High => map.high.as_ref(),
    }
}

/// Decide thinking and the effort word from what was asked. A level the
/// profile does not map is not sent, and says so.
fn reasoning(
    profile: &AiModelToolProfile,
    effort: Option<ReasoningEffort>,
    thinking_off: bool,
    notes: &mut Vec<ProfileNote>,
) -> (bool, Option<String>) {
    let (asked, setting) = if thinking_off {
        ("off", profile.reasoning.effort_map.off.as_ref())
    } else if let Some(effort) = effort {
        (level_name(effort), mapped(profile, effort))
    } else {
        return (true, None);
    };
    let channel = profile.reasoning.effort_channel;
    if channel == AiModelEffortChannel::None {
        notes.push(ProfileNote::new(
            "reasoning_effort_dropped",
            format!(
                "Reasoning level {asked:?} was not sent: profile {} has no channel for it.",
                profile.id
            ),
        ));
        return (true, None);
    }
    let Some(setting) = setting else {
        notes.push(ProfileNote::new(
            "reasoning_effort_unmapped",
            format!(
                "Reasoning level {asked:?} is not mapped by profile {}; nothing was sent for it.",
                profile.id
            ),
        ));
        return (true, None);
    };
    (setting.thinking, setting.effort.as_ref().cloned())
}

fn choose_tool_choice(
    profile: &AiModelToolProfile,
    asked: &ToolChoice,
    notes: &mut Vec<ProfileNote>,
) -> Value {
    let (effective, mut downgraded) = policy_tool_choice(profile, asked);
    notes.append(&mut downgraded);
    wire_tool_choice(&effective)
}

/// The tool choice the profile lets a request carry: what was asked, or
/// `Auto` with a `tool_choice_downgraded` note when the profile allows auto
/// only. Lane-neutral; each lane writes the result in its own wire shape.
pub fn policy_tool_choice(
    profile: &AiModelToolProfile,
    asked: &ToolChoice,
) -> (ToolChoice, Vec<ProfileNote>) {
    let mut notes = vec![];
    if *asked == ToolChoice::Auto {
        return (ToolChoice::Auto, notes);
    }
    if profile.tool_choice == AiModelToolChoicePolicy::AutoOnly {
        notes.push(ProfileNote::new(
            "tool_choice_downgraded",
            format!(
                "tool_choice {} was sent as \"auto\": profile {} allows auto only.",
                match asked {
                    ToolChoice::Required => "\"required\"".to_owned(),
                    ToolChoice::Named(name) => format!("naming {name:?}"),
                    ToolChoice::Auto => unreachable!("handled above"),
                },
                profile.id
            ),
        ));
        return (ToolChoice::Auto, notes);
    }
    (asked.clone(), notes)
}

/// The wire value of a `tool_choice` request, with no policy applied.
pub fn wire_tool_choice(asked: &ToolChoice) -> Value {
    match asked {
        ToolChoice::Auto => json!("auto"),
        ToolChoice::Required => json!("required"),
        ToolChoice::Named(name) => json!({"type":"function","function":{"name":name}}),
    }
}

/// Resolve the settings of one Chat Completions request under `profile`.
///
/// The profile's sampling value wins over `temperature` (a provider-level
/// default); `temperature` is used only for a field the chosen set leaves
/// open. The effort word never travels as OpenAI's `reasoning_effort` field
/// unless the profile's channel is the effort field.
pub fn resolve_request_settings(
    profile: &AiModelToolProfile,
    effort: Option<ReasoningEffort>,
    thinking_off: bool,
    temperature: Option<f64>,
    tools_present: bool,
    asked_choice: &ToolChoice,
) -> RequestSettings {
    let mut notes = vec![];
    let (thinking, word) = reasoning(profile, effort, thinking_off, &mut notes);
    let mut fields = Map::new();
    if let Some(value) = temperature {
        fields.insert("temperature".into(), json!(value));
    }
    sampling_fields(
        if thinking {
            &profile.sampling.thinking
        } else {
            &profile.sampling.non_thinking
        },
        &mut fields,
    );
    let mut kwargs: Map<String, Value> = profile
        .body_extras
        .chat_template_kwargs
        .as_ref()
        .map(|extras| extras.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default();
    match profile.reasoning.effort_channel {
        AiModelEffortChannel::TemplateOption => {
            if !thinking {
                kwargs.insert(THINKING_TEMPLATE_KEY.into(), json!(false));
            } else if let Some(word) = &word {
                kwargs.insert(QWEN_EFFORT_TEMPLATE_KEY.into(), json!(word));
            }
        }
        AiModelEffortChannel::EffortField => {
            if thinking && let Some(word) = &word {
                fields.insert("reasoning_effort".into(), json!(word));
            }
        }
        AiModelEffortChannel::None => {}
    }
    if !kwargs.is_empty() {
        fields.insert("chat_template_kwargs".into(), Value::Object(kwargs));
    }
    if tools_present && let Some(parallel) = profile.body_extras.parallel_tool_calls.as_ref() {
        fields.insert("parallel_tool_calls".into(), json!(*parallel));
    }
    let tool_choice = tools_present.then(|| choose_tool_choice(profile, asked_choice, &mut notes));
    RequestSettings {
        thinking,
        fields,
        tool_choice,
        notes,
    }
}
