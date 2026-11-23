//! The built-in profiles. Values marked "assumption" in the spec stay
//! assumptions until experiments X5 and X8 settle them.

use agentkit_contracts::{
    AiModelBodyExtras, AiModelDialect, AiModelEffortChannel, AiModelEffortMap,
    AiModelEffortSetting, AiModelFamily, AiModelImagePosition, AiModelLimitSource, AiModelLimits,
    AiModelLoadMode, AiModelMessageRules, AiModelReasoning, AiModelRecovery, AiModelSampling,
    AiModelSamplingSet, AiModelToolChoicePolicy, AiModelToolProfile, Optional,
};
use std::collections::BTreeMap;

pub const ANTHROPIC_PROFILE_ID: &str = "anthropic";
pub const OPENAI_PROFILE_ID: &str = "openai";
pub const GENERIC_PROFILE_ID: &str = "generic";
pub const QWEN_PROFILE_ID: &str = "qwen3.8-27b";

const CODING: [&str; 8] = [
    "read_file",
    "write_file",
    "edit_file",
    "list_dir",
    "find_files",
    "code_search",
    "run_command",
    "git_inspect",
];
const META: [&str; 3] = ["tool_search", "tool_describe", "toolset_load"];

fn names(groups: &[&[&str]]) -> Vec<String> {
    groups
        .iter()
        .flat_map(|group| group.iter())
        .map(|name| (*name).to_owned())
        .collect()
}

fn dialect(id: &str) -> AiModelDialect {
    AiModelDialect {
        id: id.into(),
        version: "1".into(),
        extra: BTreeMap::new(),
    }
}

fn setting(thinking: bool, effort: Option<&str>) -> Optional<AiModelEffortSetting> {
    Optional::Present(AiModelEffortSetting {
        thinking,
        effort: effort.map_or(Optional::Absent, |word| Optional::Present(word.into())),
        extra: BTreeMap::new(),
    })
}

fn sampling_set(
    temperature: f64,
    top_p: f64,
    top_k: f64,
    presence_penalty: Option<f64>,
) -> AiModelSamplingSet {
    AiModelSamplingSet {
        temperature: Optional::Present(temperature),
        top_p: Optional::Present(top_p),
        top_k: Optional::Present(top_k),
        presence_penalty: presence_penalty.map_or(Optional::Absent, Optional::Present),
        ..Default::default()
    }
}

fn limits(context_window: f64, max_output: f64, source: AiModelLimitSource) -> AiModelLimits {
    AiModelLimits {
        context_window,
        max_output,
        source,
        extra: BTreeMap::new(),
    }
}

fn base(id: &str, family: AiModelFamily, dialect_id: &str) -> AiModelToolProfile {
    AiModelToolProfile {
        id: id.into(),
        family,
        dialect: dialect(dialect_id),
        direct_tools: names(&[&CODING, &META]),
        max_visible_tools: 12.0,
        load_mode: AiModelLoadMode::Indirect,
        tool_choice: AiModelToolChoicePolicy::AutoOnly,
        sampling: AiModelSampling {
            thinking: AiModelSamplingSet::default(),
            non_thinking: AiModelSamplingSet::default(),
            extra: BTreeMap::new(),
        },
        reasoning: AiModelReasoning {
            effort_channel: AiModelEffortChannel::None,
            effort_map: AiModelEffortMap {
                off: Optional::Absent,
                minimal: Optional::Absent,
                low: Optional::Absent,
                medium: Optional::Absent,
                high: Optional::Absent,
                extra: BTreeMap::new(),
            },
            send_back: false,
            extra: BTreeMap::new(),
        },
        body_extras: AiModelBodyExtras::default(),
        message_rules: AiModelMessageRules {
            system_first_only: false,
            escape_tool_result_tags: false,
            image_positions: vec![AiModelImagePosition::User, AiModelImagePosition::ToolResult],
            extra: BTreeMap::new(),
        },
        limits: limits(32768.0, 4096.0, AiModelLimitSource::Estimated),
        recovery: AiModelRecovery {
            salvage: false,
            nudge: false,
            extra: BTreeMap::new(),
        },
        qualified_toolsets: vec![],
        extra: BTreeMap::new(),
    }
}

/// The profile for any model nothing else matches: the common coding shape,
/// a small direct set, conservative limits, nothing forced.
fn generic() -> AiModelToolProfile {
    let mut profile = base(GENERIC_PROFILE_ID, AiModelFamily::Generic, "common");
    profile.direct_tools = names(&[
        &[
            "read_file",
            "write_file",
            "edit_file",
            "list_dir",
            "code_search",
            "find_files",
            "run_command",
        ],
        &META,
    ]);
    // Seven direct coding tools plus the four meta tools, which take slots
    // once anything is deferred; a smaller bound demotes the last direct
    // tools. `list_dir` and `write_file` are direct because tool descriptions
    // name them and models call them by name.
    profile.max_visible_tools = 11.0;
    profile.message_rules.system_first_only = true;
    profile.message_rules.image_positions = vec![AiModelImagePosition::User];
    profile
}

/// Hosted lanes: provider limits are looked up, so the defaults are estimates.
fn hosted(
    id: &str,
    family: AiModelFamily,
    dialect_id: &str,
    context_window: f64,
    max_output: f64,
) -> AiModelToolProfile {
    let mut profile = base(id, family, dialect_id);
    profile.direct_tools = names(&[
        &CODING,
        &["run_tests", "repo_map", "submit_plan"],
        &["tool_search", "tool_describe", "tool_call", "toolset_load"],
    ]);
    profile.max_visible_tools = 24.0;
    profile.tool_choice = AiModelToolChoicePolicy::AutoRequiredNamed;
    profile.reasoning.effort_channel = AiModelEffortChannel::EffortField;
    profile.reasoning.effort_map = AiModelEffortMap {
        off: setting(false, None),
        minimal: setting(true, Some("minimal")),
        low: setting(true, Some("low")),
        medium: setting(true, Some("medium")),
        high: setting(true, Some("high")),
        extra: BTreeMap::new(),
    };
    profile.limits = limits(context_window, max_output, AiModelLimitSource::Estimated);
    profile
}

/// Qwen 3.8 27B, with the values the spec's "Profile values" table gives.
/// The limits are the owner's served values (context 131,072 to start), so
/// they carry the `user` source and stay valid once the model is qualified.
fn qwen() -> AiModelToolProfile {
    let mut profile = base(QWEN_PROFILE_ID, AiModelFamily::Qwen, "common");
    profile.direct_tools = names(&[&CODING, &META]);
    profile.max_visible_tools = 12.0;
    profile.load_mode = AiModelLoadMode::Promote;
    profile.tool_choice = AiModelToolChoicePolicy::AutoOnly;
    profile.sampling = AiModelSampling {
        thinking: sampling_set(1.0, 0.95, 20.0, None),
        non_thinking: sampling_set(0.7, 0.8, 20.0, Some(1.5)),
        extra: BTreeMap::new(),
    };
    profile.reasoning = AiModelReasoning {
        // The effort word is a template option, never OpenAI's effort field.
        effort_channel: AiModelEffortChannel::TemplateOption,
        effort_map: AiModelEffortMap {
            off: setting(false, None),
            minimal: Optional::Absent,
            low: setting(true, Some("low")),
            medium: setting(true, Some("medium")),
            high: setting(true, Some("xhigh")),
            extra: BTreeMap::new(),
        },
        send_back: true,
        extra: BTreeMap::new(),
    };
    profile.message_rules = AiModelMessageRules {
        system_first_only: true,
        escape_tool_result_tags: true,
        image_positions: vec![AiModelImagePosition::User],
        extra: BTreeMap::new(),
    };
    profile.limits = limits(131072.0, 32768.0, AiModelLimitSource::User);
    profile.recovery = AiModelRecovery {
        salvage: true,
        nudge: false,
        extra: BTreeMap::new(),
    };
    profile
}

/// The built-in profile with this id, or `None`.
pub fn builtin_profile(id: &str) -> Option<AiModelToolProfile> {
    match id {
        ANTHROPIC_PROFILE_ID => Some(hosted(
            ANTHROPIC_PROFILE_ID,
            AiModelFamily::Anthropic,
            "anthropic",
            200_000.0,
            32_000.0,
        )),
        OPENAI_PROFILE_ID => Some(hosted(
            OPENAI_PROFILE_ID,
            AiModelFamily::Openai,
            "openai",
            128_000.0,
            16_384.0,
        )),
        GENERIC_PROFILE_ID => Some(generic()),
        QWEN_PROFILE_ID => Some(qwen()),
        _ => None,
    }
}
