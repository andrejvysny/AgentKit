use agentkit_contracts::*;
use serde_json::{Value, json};

fn digest(seed: char) -> String {
    seed.to_string().repeat(64)
}

fn prefix() -> Value {
    json!({
        "revision": 1,
        "toolsDigest": digest('a'),
        "systemDigest": digest('b'),
        "rulesDigest": digest('c'),
        "memoryCoreDigest": digest('d'),
        "paramsDigest": digest('e'),
        "frozenAtTask": "task-1",
        "cacheKey": "abcdefghijklmnopqrstuvwxyz234567",
    })
}

#[test]
fn a_minimal_prefix_validates_and_round_trips_without_the_tool_track_fields() {
    let value = prefix();
    validate_schema("AiSessionPrefixSchema", &value).unwrap();
    let parsed = AiSessionPrefix::from_json(value.clone()).unwrap();
    assert_eq!(parsed.tool_manifest_hash, Optional::Absent);
    assert_eq!(serde_json::to_value(&parsed).unwrap(), value);
}

#[test]
fn tool_track_fields_are_optional_and_round_trip() {
    let mut value = prefix();
    value["toolManifestHash"] = json!(digest('f'));
    value["modelProfileId"] = json!("profile-1");
    value["dialectVersion"] = json!("1");
    validate_schema("AiSessionPrefixSchema", &value).unwrap();
    let parsed = AiSessionPrefix::from_json(value.clone()).unwrap();
    assert_eq!(
        parsed.model_profile_id,
        Optional::Present("profile-1".into())
    );
    assert_eq!(serde_json::to_value(&parsed).unwrap(), value);
}

#[test]
fn malformed_prefixes_are_refused() {
    for (field, bad) in [
        ("toolsDigest", json!("not-a-digest")),
        ("systemDigest", json!(digest('A'))),
        ("revision", json!(0)),
        ("revision", json!(1.5)),
        ("cacheKey", json!("")),
        ("frozenAtTask", json!(1)),
        ("toolManifestHash", json!("short")),
    ] {
        let mut value = prefix();
        value[field] = bad;
        assert!(
            validate_schema("AiSessionPrefixSchema", &value).is_err(),
            "{field}"
        );
    }
    let mut missing = prefix();
    missing.as_object_mut().unwrap().remove("paramsDigest");
    assert!(validate_schema("AiSessionPrefixSchema", &missing).is_err());
}

fn tool(name: &str) -> Value {
    json!({"name": name, "version": "1", "effect": "read", "capability": "c",
        "description": "d", "inputSchema": {"type": "object"}})
}

#[test]
fn the_tool_manifest_and_deferred_catalog_validate_and_round_trip() {
    let mut value = prefix();
    value["toolManifestHash"] = json!(digest('f'));
    value["toolManifest"] = json!([tool("a"), tool("b")]);
    value["deferredTools"] = json!([tool("late")]);
    validate_schema("AiSessionPrefixSchema", &value).unwrap();
    let parsed = AiSessionPrefix::from_json(value.clone()).unwrap();
    let Optional::Present(manifest) = &parsed.tool_manifest else {
        panic!("manifest present");
    };
    assert_eq!(manifest.len(), 2);
    assert_eq!(serde_json::to_value(&parsed).unwrap(), value);
}

#[test]
fn a_malformed_manifest_entry_is_refused() {
    for bad in [json!("a"), json!([{"name": "a"}]), json!({"a": 1})] {
        for field in ["toolManifest", "deferredTools"] {
            let mut value = prefix();
            value[field] = bad.clone();
            assert!(
                validate_schema("AiSessionPrefixSchema", &value).is_err(),
                "{field}"
            );
        }
    }
}
