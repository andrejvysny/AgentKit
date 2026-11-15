use agentkit_contracts::*;
use serde_json::{Value, json};

fn profile() -> Value {
    json!({
        "id": "p",
        "family": "qwen",
        "dialect": {"id": "common", "version": "1"},
        "directTools": ["read_file"],
        "maxVisibleTools": 12,
        "loadMode": "promote",
        "toolChoice": "auto_only",
        "sampling": {
            "thinking": {"temperature": 1, "topP": 0.95, "topK": 20},
            "nonThinking": {"temperature": 0.7, "topP": 0.8, "topK": 20, "presencePenalty": 1.5}
        },
        "reasoning": {
            "effortChannel": "template_option",
            "effortMap": {
                "off": {"thinking": false},
                "high": {"thinking": true, "effort": "xhigh"}
            },
            "sendBack": true
        },
        "bodyExtras": {"chat_template_kwargs": {"a": 1}, "parallel_tool_calls": false},
        "messageRules": {
            "systemFirstOnly": true,
            "escapeToolResultTags": true,
            "imagePositions": ["user"]
        },
        "limits": {"contextWindow": 131072, "maxOutput": 32768, "source": "user"},
        "recovery": {"salvage": true, "nudge": false},
        "qualifiedToolsets": []
    })
}

#[test]
fn a_profile_validates_and_round_trips() {
    let value = profile();
    validate_schema("AiModelToolProfileSchema", &value).unwrap();
    let parsed = AiModelToolProfile::from_json(value.clone()).unwrap();
    assert_eq!(parsed.family, AiModelFamily::Qwen);
    assert_eq!(parsed.limits.source, AiModelLimitSource::User);
    assert_eq!(serde_json::to_value(&parsed).unwrap(), value);
}

#[test]
fn malformed_profiles_are_refused() {
    for (field, bad) in [
        ("family", json!("llama")),
        ("loadMode", json!("eager")),
        ("toolChoice", json!("required")),
        ("maxVisibleTools", json!(0)),
        ("directTools", json!([""])),
        (
            "limits",
            json!({"contextWindow": 1, "maxOutput": 1, "source": "guess"}),
        ),
        ("bodyExtras", json!({"tools": []})),
        ("bodyExtras", json!({"parallel_tool_calls": "yes"})),
        (
            "messageRules",
            json!({"systemFirstOnly": true, "escapeToolResultTags": true, "imagePositions": ["system"]}),
        ),
        (
            "reasoning",
            json!({"effortChannel": "body", "effortMap": {}, "sendBack": true}),
        ),
    ] {
        let mut value = profile();
        value[field] = bad;
        assert!(
            validate_schema("AiModelToolProfileSchema", &value).is_err(),
            "{field}"
        );
    }
    let mut missing = profile();
    missing.as_object_mut().unwrap().remove("recovery");
    assert!(validate_schema("AiModelToolProfileSchema", &missing).is_err());
}

#[test]
fn the_body_extras_decoder_is_closed() {
    assert!(AiModelBodyExtras::from_json(json!({"stop": []})).is_err());
    assert!(AiModelBodyExtras::from_json(json!({})).is_ok());
}

#[test]
fn the_prefix_carries_the_profile_digest() {
    let digest = "a".repeat(64);
    let mut value = json!({
        "revision": 1,
        "toolsDigest": digest, "systemDigest": digest, "rulesDigest": digest,
        "memoryCoreDigest": digest, "paramsDigest": digest,
        "frozenAtTask": "t", "cacheKey": "k",
        "modelProfileId": "p", "dialectVersion": "1", "modelProfileDigest": digest
    });
    validate_schema("AiSessionPrefixSchema", &value).unwrap();
    let parsed = AiSessionPrefix::from_json(value.clone()).unwrap();
    assert_eq!(parsed.model_profile_digest, Optional::Present(digest));
    assert_eq!(serde_json::to_value(&parsed).unwrap(), value);
    value["modelProfileDigest"] = json!("short");
    assert!(validate_schema("AiSessionPrefixSchema", &value).is_err());
}
