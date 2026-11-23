//! Model tool profiles (T2.1, TU-15, TU-16, TU-19, TU-22): the built-in
//! values, resolution order, the body-extras allow-list, and freezing.

use agentkit_contracts::{
    AiModelEffortChannel, AiModelFamily, AiModelLimitSource, AiModelLoadMode,
    AiModelToolChoicePolicy, AiSessionPrefix, Optional,
};
use agentkit_core::{
    AiModelToolProfile, GENERIC_PROFILE_ID, LimitFacts, META_TOOLS, ModelFacts, PrefixDigests,
    ProfileError, ProfileQuery, PromptLayout, QWEN_PROFILE_ID, builtin_profile, freeze,
    frozen_profile, is_meta_tool, model_profile_digest, resolve_profile, stamp_model_profile,
    validate_profile_json,
};
use serde_json::{Value, json};

const QWEN_GOLDEN: &str = include_str!("fixtures/model-profile-qwen3.8-27b.json");

fn query<'a>(provider: &'a str, model: &'a str) -> ProfileQuery<'a> {
    ProfileQuery {
        provider,
        model,
        facts: ModelFacts::default(),
        user_profile: None,
    }
}

fn facts(context: u64, output: Option<u64>) -> Option<LimitFacts> {
    Some(LimitFacts {
        context_window: Some(context),
        max_output: output,
    })
}

#[test]
fn the_qwen_profile_carries_exactly_the_spec_values() {
    let profile = builtin_profile(QWEN_PROFILE_ID).expect("built in");
    let golden: Value = serde_json::from_str(QWEN_GOLDEN).unwrap();
    assert_eq!(serde_json::to_value(&profile).unwrap(), golden);
    // The golden is also valid configuration.
    assert_eq!(validate_profile_json(&golden).unwrap(), profile);

    assert_eq!(profile.family, AiModelFamily::Qwen);
    assert_eq!(profile.load_mode, AiModelLoadMode::Promote);
    assert_eq!(profile.tool_choice, AiModelToolChoicePolicy::AutoOnly);
    assert!(profile.direct_tools.len() <= 12);
    assert_eq!(profile.limits.context_window, 131072.0);
    assert_eq!(profile.limits.max_output, 32768.0);
    assert_eq!(
        profile.reasoning.effort_channel,
        AiModelEffortChannel::TemplateOption
    );
    assert!(profile.reasoning.send_back);
    assert!(profile.recovery.salvage && !profile.recovery.nudge);
    assert!(profile.message_rules.system_first_only);
    assert!(profile.message_rules.escape_tool_result_tags);
    // Off means thinking disabled, with the non-thinking sampling set.
    let off = profile.reasoning.effort_map.off.as_ref().unwrap();
    assert!(!off.thinking && off.effort.is_absent());
    // High is sent as xhigh; the others keep their names.
    let high = profile.reasoning.effort_map.high.as_ref().unwrap();
    assert_eq!(high.effort, Optional::Present("xhigh".into()));
    assert!(profile.body_extras.parallel_tool_calls.is_absent());
}

#[test]
fn every_builtin_profile_is_valid_configuration() {
    for id in ["anthropic", "openai", "generic", QWEN_PROFILE_ID] {
        let profile = builtin_profile(id).expect(id);
        assert_eq!(profile.id, id);
        let value = serde_json::to_value(&profile).unwrap();
        assert_eq!(validate_profile_json(&value).unwrap(), profile, "{id}");
        assert!(
            profile.direct_tools.len() as f64 <= profile.max_visible_tools,
            "{id}"
        );
    }
    assert!(builtin_profile("nope").is_none());
}

/// Once anything is deferred the meta tools take visible slots, so the
/// bound must hold every non-meta direct tool plus all of them; otherwise
/// the last direct tools are silently demoted.
#[test]
fn every_builtin_profile_shows_all_its_direct_tools() {
    for id in ["anthropic", "openai", "generic", QWEN_PROFILE_ID] {
        let profile = builtin_profile(id).expect(id);
        let direct = profile
            .direct_tools
            .iter()
            .filter(|name| !is_meta_tool(name))
            .count();
        assert!(
            (direct + META_TOOLS.len()) as f64 <= profile.max_visible_tools,
            "{id}: {direct} direct + {} meta > {}",
            META_TOOLS.len(),
            profile.max_visible_tools
        );
    }
}

#[test]
fn the_generic_profile_is_conservative() {
    let generic = builtin_profile(GENERIC_PROFILE_ID).unwrap();
    assert_eq!(generic.family, AiModelFamily::Generic);
    assert_eq!(generic.tool_choice, AiModelToolChoicePolicy::AutoOnly);
    assert_eq!(generic.load_mode, AiModelLoadMode::Indirect);
    // Smaller than the Qwen profile's set, but with every tool that tool
    // descriptions name, so the model can call them directly.
    let qwen = builtin_profile(QWEN_PROFILE_ID).unwrap();
    assert!(generic.direct_tools.len() <= 10);
    assert!(generic.direct_tools.len() < qwen.direct_tools.len());
    for name in ["list_dir", "write_file"] {
        assert!(
            generic.direct_tools.iter().any(|tool| tool == name),
            "{name}"
        );
    }
    assert_eq!(generic.limits.source, AiModelLimitSource::Estimated);
    assert!(generic.qualified_toolsets.is_empty());
}

#[test]
fn profiles_match_by_provider_and_model_pattern() {
    for (provider, model, id) in [
        ("openai-compatible", "qwen3.8-27b", "qwen3.8-27b"),
        ("custom:litellm", "Qwen/Qwen3.8-27B-FP8", "qwen3.8-27b"),
        ("lmstudio", "hosted_vllm/qwen3.8-27b", "qwen3.8-27b"),
        ("anthropic", "claude-sonnet-5-5", "anthropic"),
        ("openai", "gpt-5.5", "openai"),
        ("openai-compatible", "gpt-5.5", "generic"),
        ("openrouter", "anthropic/claude-sonnet-5-5", "generic"),
        ("ollama", "llama3", "generic"),
        ("ollama", "qwen3.5-27b", "generic"),
    ] {
        assert_eq!(
            resolve_profile(&query(provider, model)).unwrap().id,
            id,
            "{provider} {model}"
        );
    }
}

#[test]
fn limits_resolve_user_then_proxy_then_server_then_probe_then_defaults() {
    let mut q = query("openai-compatible", "qwen3.8-27b");
    let defaults = resolve_profile(&q).unwrap();
    assert_eq!(defaults.limits.context_window, 131072.0);

    q.facts.probe = facts(40_000, Some(8_000));
    let probed = resolve_profile(&q).unwrap();
    assert_eq!(
        (
            probed.limits.context_window,
            probed.limits.max_output,
            probed.limits.source
        ),
        (40_000.0, 8_000.0, AiModelLimitSource::Probe)
    );

    q.facts.server = facts(50_000, None);
    let served = resolve_profile(&q).unwrap();
    assert_eq!(served.limits.context_window, 50_000.0);
    assert_eq!(served.limits.source, AiModelLimitSource::Server);
    assert_eq!(
        served.limits.max_output, 32768.0,
        "a missing output keeps the default"
    );

    q.facts.proxy = facts(60_000, Some(70_000));
    let proxied = resolve_profile(&q).unwrap();
    assert_eq!(proxied.limits.source, AiModelLimitSource::Proxy);
    assert_eq!(
        proxied.limits.max_output, 60_000.0,
        "output never exceeds the window"
    );

    q.facts.user = facts(65_536, Some(4096));
    let configured = resolve_profile(&q).unwrap();
    assert_eq!(
        (
            configured.limits.context_window,
            configured.limits.max_output,
            configured.limits.source
        ),
        (65_536.0, 4096.0, AiModelLimitSource::User)
    );

    // A source without a context window says nothing.
    let mut empty = query("openai-compatible", "qwen3.8-27b");
    empty.facts.proxy = Some(LimitFacts {
        context_window: None,
        max_output: Some(1),
    });
    assert_eq!(resolve_profile(&empty).unwrap(), defaults);
}

#[test]
fn a_litellm_list_without_a_context_field_resolves_through_the_other_sources() {
    // TU-19: the server list has no window, model information does.
    let mut q = query("openai-compatible", "qwen3.8-27b");
    q.facts.server = Some(LimitFacts::default());
    q.facts.proxy = facts(131_072, Some(32_768));
    let profile = resolve_profile(&q).unwrap();
    assert_eq!(profile.limits.source, AiModelLimitSource::Proxy);
    assert_ne!(profile.limits.source, AiModelLimitSource::Estimated);
}

#[test]
fn a_user_profile_replaces_the_match_and_outranks_looked_up_limits() {
    let mut record = serde_json::to_value(builtin_profile(QWEN_PROFILE_ID).unwrap()).unwrap();
    record["id"] = json!("my-qwen");
    record["limits"] = json!({"contextWindow": 90000, "maxOutput": 9000, "source": "user"});
    let mut q = query("openai-compatible", "anything");
    q.user_profile = Some(&record);
    q.facts.proxy = facts(262_144, None);
    let profile = resolve_profile(&q).unwrap();
    assert_eq!(profile.id, "my-qwen");
    assert_eq!(profile.limits.context_window, 90_000.0);
    assert_eq!(profile.limits.source, AiModelLimitSource::User);
}

#[test]
fn estimated_limits_are_refused_for_a_qualified_model() {
    let mut record = serde_json::to_value(builtin_profile(GENERIC_PROFILE_ID).unwrap()).unwrap();
    assert!(
        validate_profile_json(&record).is_ok(),
        "unqualified may estimate"
    );
    record["qualifiedToolsets"] = json!(["files"]);
    assert_eq!(
        validate_profile_json(&record),
        Err(ProfileError::EstimatedLimitForQualified {
            profile: "generic".into()
        })
    );
    record["limits"]["source"] = json!("probe");
    assert!(validate_profile_json(&record).is_ok());
}

#[test]
fn a_body_extra_outside_the_allow_list_is_rejected_at_configuration() {
    let mut record = serde_json::to_value(builtin_profile(QWEN_PROFILE_ID).unwrap()).unwrap();
    record["bodyExtras"] = json!({
        "chat_template_kwargs": {"enable_thinking": true},
        "parallel_tool_calls": false
    });
    let profile = validate_profile_json(&record).expect("allow-listed fields pass");
    assert_eq!(
        profile.body_extras.parallel_tool_calls,
        Optional::Present(false)
    );

    for field in [
        "tools",
        "tool_choice",
        "model",
        "messages",
        "stream",
        "logit_bias",
    ] {
        record["bodyExtras"] = json!({ field: 1 });
        assert_eq!(
            validate_profile_json(&record),
            Err(ProfileError::BodyExtraNotAllowed(field.into())),
            "{field}"
        );
    }
}

#[test]
fn malformed_records_are_refused() {
    let base = serde_json::to_value(builtin_profile(QWEN_PROFILE_ID).unwrap()).unwrap();
    let edits: [(&str, Value); 5] = [
        ("family", json!("llama")),
        ("loadMode", json!("eager")),
        ("maxVisibleTools", json!(0)),
        (
            "limits",
            json!({"contextWindow": 10, "maxOutput": 20, "source": "user"}),
        ),
        (
            "directTools",
            json!([
                "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m"
            ]),
        ),
    ];
    for (field, bad) in edits {
        let mut record = base.clone();
        record[field] = bad;
        assert!(validate_profile_json(&record).is_err(), "{field}");
    }
    let mut thinking_off = base.clone();
    thinking_off["reasoning"]["effortMap"]["off"] = json!({"thinking": false, "effort": "low"});
    assert!(validate_profile_json(&thinking_off).is_err());
    let mut unit = base;
    unit["sampling"]["thinking"]["topP"] = json!(1.5);
    assert!(validate_profile_json(&unit).is_err());
}

fn first_prefix(profile: &AiModelToolProfile) -> AiSessionPrefix {
    let digests = PrefixDigests::compute(&PromptLayout::of(Some("s"), None, None), &[], &json!({}));
    let mut prefix = freeze(None, digests, "chat", "", "task-1").prefix().clone();
    assert!(stamp_model_profile(&mut prefix, profile));
    prefix
}

#[test]
fn the_prefix_records_profile_id_dialect_version_and_digest() {
    let profile = builtin_profile(QWEN_PROFILE_ID).unwrap();
    let prefix = first_prefix(&profile);
    assert_eq!(
        prefix.model_profile_id,
        Optional::Present("qwen3.8-27b".into())
    );
    assert_eq!(prefix.dialect_version, Optional::Present("1".into()));
    assert_eq!(
        prefix.model_profile_digest,
        Optional::Present(model_profile_digest(&profile))
    );
    let mut again = prefix.clone();
    assert!(
        !stamp_model_profile(&mut again, &profile),
        "stamping is idempotent"
    );
    assert_eq!(again, prefix);
}

#[test]
fn the_digest_follows_the_record() {
    let qwen = builtin_profile(QWEN_PROFILE_ID).unwrap();
    let mut edited = qwen.clone();
    edited.limits.context_window = 65_536.0;
    assert_eq!(
        model_profile_digest(&qwen),
        model_profile_digest(&qwen.clone())
    );
    assert_ne!(model_profile_digest(&qwen), model_profile_digest(&edited));
}

#[test]
fn a_changed_profile_does_not_reach_a_running_session() {
    // The session froze the profile at start; later resolution returns a
    // different record, yet the session keeps the frozen one.
    let (frozen, created) = frozen_profile(None, || {
        resolve_profile(&query("openai-compatible", "qwen3.8-27b"))
    })
    .unwrap();
    assert!(created);

    let mut changed_settings = query("openai-compatible", "qwen3.8-27b");
    changed_settings.facts.user = facts(65_536, Some(4096));
    let newer = resolve_profile(&changed_settings).unwrap();
    assert_ne!(newer, frozen);

    let (kept, created) = frozen_profile(Some(frozen.clone()), || Ok(newer.clone())).unwrap();
    assert!(!created, "the resolver is not even consulted");
    assert_eq!(kept, frozen);

    // And the recorded prefix stays on the frozen digest across a re-freeze.
    let prefix = first_prefix(&frozen);
    let digests =
        PrefixDigests::compute(&PromptLayout::of(Some("s2"), None, None), &[], &json!({}));
    let refrozen = freeze(Some(&prefix), digests, "chat", "", "task-2");
    assert_eq!(
        refrozen.prefix().model_profile_digest,
        prefix.model_profile_digest
    );
    assert_eq!(refrozen.prefix().model_profile_id, prefix.model_profile_id);
}
