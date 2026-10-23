use super::support::*;
use agentkit_host::*;
use serde_json::{Value, json};

#[test]
fn native_write_fingerprint_is_versioned_locale_free_and_excludes_action_id() {
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/locale-fingerprints.json")).unwrap();
    assert_ne!(
        fixture["evidence"][0]["fingerprint"],
        fixture["evidence"][1]["fingerprint"]
    );
    assert_eq!(
        native_write_fingerprint(&fixture["input"]),
        fixture["nativeFingerprint"].as_str().unwrap()
    );
    assert_eq!(
        native_write_fingerprint(&json!({"z":2,"ä":1,"action_id":"ignored"})),
        fixture["nativeFingerprint"].as_str().unwrap()
    );
    assert_eq!(NATIVE_GOVERNANCE_IDENTITY_VERSION, "native-governance-v1");
}

#[test]
fn governance_fingerprint_uses_javascript_serialization_for_overflowing_numbers() {
    let input = agentkit_contracts::parse_javascript_json(
        r#"{"positive":1e400,"negative":-1e400,"action_id":"ignored"}"#,
    )
    .unwrap();
    assert_eq!(
        native_write_fingerprint(&input),
        native_write_fingerprint(&json!({"positive":null,"negative":null}))
    );
    assert_eq!(
        agentkit_contracts::javascript_number(&input["positive"]),
        Some(f64::INFINITY)
    );
}

#[test]
fn policy_defaults_to_confirmation_and_scopes_risk_chat_and_tool() {
    let policy = SessionWritePolicy::default();
    let query = query();
    assert!(!policy.is_auto_apply_allowed(&query));
    let key = policy.allow(grant(&query)).unwrap();
    assert!(policy.is_auto_apply_allowed(&query));
    for changed in 0..4 {
        let mut other = query.clone();
        match changed {
            0 => other.risk = WriteRisk::Destructive,
            1 => other.scope_key = "other-document".into(),
            2 => other.chat_id = "other-chat".into(),
            _ => other.tool_name = "other-tool".into(),
        }
        assert!(!policy.is_auto_apply_allowed(&other));
    }
    policy.revoke("other-chat", &key).unwrap();
    assert!(policy.is_auto_apply_allowed(&query));
    policy.revoke("chat", &key).unwrap();
    assert!(!policy.is_auto_apply_allowed(&query));
}

#[test]
fn inbound_actor_never_inherits_auto_all_or_chat_grants() {
    let policy = SessionWritePolicy::new(WritePolicyMode::AutoAll);
    let mut query = query();
    policy.allow(grant(&query)).unwrap();
    query.actor_id = Some("inbound-session".into());
    assert!(!policy.is_auto_apply_allowed(&query));
    policy.allow(grant(&query)).unwrap();
    assert!(policy.is_auto_apply_allowed(&query));
    for changed in 0..3 {
        let mut other = query.clone();
        match changed {
            0 => other.actor_id = Some("other-session".into()),
            1 => other.payload_fingerprint = "other-payload".into(),
            _ => other.revision = Some("2".into()),
        }
        assert!(!policy.is_auto_apply_allowed(&other));
    }
    policy.revoke_actor("inbound-session").unwrap();
    assert!(!policy.is_auto_apply_allowed(&query));
}

#[test]
fn confirm_all_overrides_allowances_and_missing_actor_scope_is_rejected() {
    let policy = SessionWritePolicy::default();
    let mut query = query();
    let mut broad = grant(&query);
    broad.scope_key = None;
    policy.allow(broad).unwrap();
    query.scope_key = "another-document".into();
    assert!(policy.is_auto_apply_allowed(&query));
    policy.set_mode(WritePolicyMode::ConfirmAllWrites).unwrap();
    assert!(!policy.is_auto_apply_allowed(&query));
    query.actor_id = Some("session".into());
    let mut invalid = grant(&query);
    invalid.scope_key = None;
    assert!(policy.allow(invalid).is_err());
    let mut invalid = grant(&query);
    invalid.payload_fingerprint = None;
    assert!(policy.allow(invalid).is_err());
}
