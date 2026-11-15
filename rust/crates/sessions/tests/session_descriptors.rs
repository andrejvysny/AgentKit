//! D3: the session-descriptor parser matrix against the frozen TypeScript
//! authority (`packages/host/src/sessions/descriptor.ts`,
//! `isValidSessionTurnKind` in `packages/host/src/tasks/kinds.ts`).
//!
//! Invariants under test: absent optional fields are accepted with defaults
//! and stay omitted; malformed-present values (including explicit `null`)
//! REJECT the whole descriptor instead of being silently dropped; the
//! custom-kind grammar `^[a-z][a-z0-9_-]*(\.[a-z0-9_-]+)*$` accepts
//! `project.1` and `project._worker`.

use agentkit_sessions::{
    CHAT_TURN_TASK_KIND, REASONING_EFFORTS, is_valid_session_turn_kind, read_session_descriptor,
    session_content_selection, session_turn_kind,
};
use serde_json::{Value, json};

const CREATED_AT: &str = "2026-01-01T00:00:00.000Z";

fn metadata(descriptor: Value) -> Value {
    json!({ "agentkit.session": descriptor })
}

fn base() -> Value {
    json!({"version": 1, "profile": {"id": "p1"}, "createdAt": CREATED_AT})
}

/// The minimal valid descriptor merged with extra top-level keys.
fn with(extra: Value) -> Value {
    let mut descriptor = base().as_object().cloned().unwrap();
    for (key, value) in extra.as_object().unwrap() {
        descriptor.insert(key.clone(), value.clone());
    }
    Value::Object(descriptor)
}

fn read(descriptor: Value) -> Option<Value> {
    read_session_descriptor(&metadata(descriptor))
}

fn accepted(descriptor: Value) -> Value {
    read(descriptor.clone())
        .unwrap_or_else(|| panic!("descriptor rejected but must be valid: {descriptor}"))
}

fn rejected(descriptor: Value, what: &str) {
    assert!(
        read(descriptor.clone()).is_none(),
        "{what}: malformed-present value silently accepted: {descriptor}"
    );
}

#[test]
fn absent_or_non_object_descriptor_reads_as_none() {
    assert_eq!(read_session_descriptor(&json!({})), None);
    for raw in [
        Value::Null,
        json!(5),
        json!("session"),
        json!([{"version": 1}]),
        json!(true),
    ] {
        assert_eq!(
            read_session_descriptor(&metadata(raw.clone())),
            None,
            "non-object descriptor {raw} accepted"
        );
    }
}

#[test]
fn minimal_valid_descriptor_keeps_absent_optionals_omitted() {
    let descriptor = accepted(base());
    assert_eq!(
        descriptor,
        json!({"version": 1, "profile": {"id": "p1"}, "createdAt": CREATED_AT})
    );
    for key in [
        "providerId",
        "model",
        "reasoningEffort",
        "workspace",
        "context",
        "skills",
        "turnKind",
    ] {
        assert!(descriptor.get(key).is_none(), "{key} present though absent");
    }
    assert!(
        descriptor["profile"].get("digest").is_none(),
        "digest present though absent"
    );
}

#[test]
fn version_must_be_the_number_one() {
    // TS: `raw["version"] !== 1` — any JSON number equal to 1 passes.
    assert!(
        read(with(json!({"version": 1.0}))).is_some(),
        "1.0 rejected"
    );
    for bad in [json!(2), json!("1"), json!(true), Value::Null, json!(1.5)] {
        rejected(with(json!({"version": bad})), "version");
    }
    let mut missing = base();
    missing.as_object_mut().unwrap().remove("version");
    rejected(missing, "missing version");
}

#[test]
fn profile_grammar() {
    for bad in [Value::Null, json!(5), json!("p"), json!(["p"])] {
        rejected(with(json!({"profile": bad})), "profile shape");
    }
    rejected(with(json!({"profile": {}})), "profile.id missing");
    rejected(with(json!({"profile": {"id": ""}})), "profile.id empty");
    rejected(with(json!({"profile": {"id": 5}})), "profile.id non-string");
    // digest: absent or string only — a malformed-present digest invalidates
    // the descriptor instead of being dropped.
    assert!(read(with(json!({"profile": {"id": "p1", "digest": ""}}))).is_some());
    rejected(
        with(json!({"profile": {"id": "p1", "digest": 5}})),
        "profile.digest scalar",
    );
    rejected(
        with(json!({"profile": {"id": "p1", "digest": null}})),
        "profile.digest explicit null",
    );
    // Extra profile keys are dropped from the normalized descriptor.
    let descriptor = accepted(with(json!({"profile": {"id": "p1", "extra": 1}})));
    assert_eq!(descriptor["profile"], json!({"id": "p1"}));
}

#[test]
fn optional_scalars_reject_malformed_present_values() {
    for key in ["providerId", "model"] {
        assert!(
            read(with(json!({ key: "value" }))).is_some(),
            "{key} string rejected"
        );
        rejected(with(json!({ key: 5 })), &format!("{key} scalar"));
        rejected(
            with(json!({ key: Value::Null })),
            &format!("{key} explicit null"),
        );
    }
    for effort in REASONING_EFFORTS {
        assert!(
            read(with(json!({"reasoningEffort": effort}))).is_some(),
            "{effort} rejected"
        );
    }
    rejected(
        with(json!({"reasoningEffort": "ultra"})),
        "reasoningEffort outside the enum",
    );
    rejected(
        with(json!({"reasoningEffort": 2})),
        "reasoningEffort scalar",
    );
    rejected(
        with(json!({"reasoningEffort": null})),
        "reasoningEffort explicit null",
    );
    // createdAt is a required string (empty allowed, matching the TS check).
    assert!(read(with(json!({"createdAt": ""}))).is_some());
    rejected(with(json!({"createdAt": 5})), "createdAt scalar");
    rejected(
        with(json!({"createdAt": Value::Null})),
        "createdAt explicit null",
    );
    let mut missing = base();
    missing.as_object_mut().unwrap().remove("createdAt");
    rejected(missing, "createdAt missing");
}

#[test]
fn explicit_nulls_reject_every_optional_object_field() {
    // Absent is valid for each optional; explicit `null` is a
    // malformed-present value (`isRecord(null)` is false in TS).
    for key in ["workspace", "context", "skills"] {
        rejected(
            with(json!({ key: Value::Null })),
            &format!("{key} explicit null"),
        );
    }
}

#[test]
fn workspace_grammar() {
    let descriptor = accepted(with(json!({
        "workspace": {"root": "/project", "trusted": true, "extra": "dropped"}
    })));
    assert_eq!(
        descriptor["workspace"],
        json!({"root": "/project", "trusted": true}),
        "workspace not normalized to {{root, trusted}}"
    );
    assert!(
        read(with(json!({
            "workspace": {"root": "/project", "trusted": false}
        })))
        .is_some()
    );
    rejected(
        with(json!({"workspace": {"root": "", "trusted": true}})),
        "workspace.root empty",
    );
    rejected(
        with(json!({"workspace": {"root": 5, "trusted": true}})),
        "workspace.root scalar",
    );
    rejected(
        with(json!({"workspace": {"root": "/project"}})),
        "workspace.trusted missing",
    );
    rejected(
        with(json!({"workspace": {"root": "/project", "trusted": "yes"}})),
        "workspace.trusted non-boolean",
    );
    rejected(
        with(json!({"workspace": {"root": "/project", "trusted": null}})),
        "workspace.trusted explicit null",
    );
    // Scalars and arrays are not records: malformed-present, not absent.
    for bad in [
        json!("/project"),
        json!(5),
        json!(true),
        json!(["/project"]),
    ] {
        rejected(with(json!({"workspace": bad})), "workspace non-record");
    }
}

#[test]
fn context_grammar() {
    assert_eq!(accepted(with(json!({"context": {}})))["context"], json!({}));
    for policy in ["final_answers", "transcript"] {
        assert_eq!(
            accepted(with(json!({"context": {"policy": policy}})))["context"],
            json!({"policy": policy})
        );
    }
    rejected(
        with(json!({"context": {"policy": "aggressive"}})),
        "context.policy outside the enum",
    );
    rejected(
        with(json!({"context": {"policy": null}})),
        "context.policy explicit null",
    );
    // A scalar (or array) context is rejected — it used to be accepted as if
    // it were merely absent.
    for bad in [json!("final_answers"), json!(5), json!(["transcript"])] {
        rejected(with(json!({"context": bad})), "context non-record");
    }
    // Positive integers survive verbatim; `1.0` is an integer in JS.
    let descriptor = accepted(with(json!({
        "context": {"windowTokens": 4096, "reserveTokens": 1024, "keepRecentTokens": 2048}
    })));
    assert_eq!(
        descriptor["context"],
        json!({"windowTokens": 4096, "reserveTokens": 1024, "keepRecentTokens": 2048})
    );
    assert!(read(with(json!({"context": {"windowTokens": 1.0}}))).is_some());
    // Token budgets: string and zero (and negative / fractional / null) are
    // malformed-present, not silently dropped.
    for bad in [json!("4096"), json!(0), json!(-1), json!(1.5), Value::Null] {
        for key in ["windowTokens", "reserveTokens", "keepRecentTokens"] {
            rejected(
                with(json!({"context": {key: bad.clone()}})),
                &format!("context.{key} = {bad}"),
            );
        }
    }
}

#[test]
fn auto_compaction_is_a_bool_switch() {
    for auto in [true, false] {
        assert_eq!(
            accepted(with(json!({"context": {"autoCompaction": auto}})))["context"],
            json!({"autoCompaction": auto})
        );
    }
    for bad in [json!("true"), json!(1), Value::Null, json!({})] {
        rejected(
            with(json!({"context": {"autoCompaction": bad.clone()}})),
            &format!("autoCompaction = {bad}"),
        );
    }
}

#[test]
fn masking_grammar() {
    let descriptor = accepted(with(json!({
        "context": {"masking": {"triggerRatio": 0.8, "targetRatio": 0.5, "minGainRatio": 0.2, "keepRecentTurns": 3, "imageMaxAgeTurns": 6}}
    })));
    assert_eq!(
        descriptor["context"]["masking"],
        json!({"triggerRatio": 0.8, "targetRatio": 0.5, "minGainRatio": 0.2, "keepRecentTurns": 3, "imageMaxAgeTurns": 6})
    );
    assert_eq!(
        accepted(with(json!({"context": {"masking": {}}})))["context"],
        json!({"masking": {}})
    );
    for bad in [json!(true), Value::Null, json!("0.8"), json!([1])] {
        rejected(
            with(json!({"context": {"masking": bad.clone()}})),
            &format!("masking = {bad}"),
        );
    }
    for bad in [json!(0), json!(1.5), json!("0.5"), Value::Null, json!(-0.1)] {
        for key in ["triggerRatio", "targetRatio", "minGainRatio"] {
            rejected(
                with(json!({"context": {"masking": {key: bad.clone()}}})),
                &format!("masking.{key} = {bad}"),
            );
        }
    }
    for bad in [json!(0), json!(1.5), json!("3"), Value::Null, json!(-5)] {
        for key in ["keepRecentTurns", "imageMaxAgeTurns"] {
            rejected(
                with(json!({"context": {"masking": {key: bad.clone()}}})),
                &format!("masking.{key} = {bad}"),
            );
        }
    }
    rejected(
        with(json!({"context": {"masking": {"triggerRatio": 0.5, "targetRatio": 0.5}}})),
        "targetRatio must stay below triggerRatio",
    );
    // The EFFECTIVE thresholds are checked: a lone trigger below the default
    // target, a lone target above the default trigger, minGain of 1.
    for bad in [
        json!({"triggerRatio": 0.4}),
        json!({"targetRatio": 0.9}),
        json!({"minGainRatio": 1}),
        json!({"enabled": "yes"}),
        json!({"enabled": null}),
    ] {
        rejected(
            with(json!({"context": {"masking": bad.clone()}})),
            &format!("masking = {bad}"),
        );
    }
    assert_eq!(
        accepted(with(json!({"context": {"masking": {"enabled": false}}})))["context"],
        json!({"masking": {"enabled": false}})
    );
}

#[test]
fn legacy_elision_opt_out_reads_as_masking_disabled() {
    assert_eq!(
        accepted(with(json!({"context": {"toolResultElision": false}})))["context"],
        json!({"masking": {"enabled": false}})
    );
    assert_eq!(
        accepted(with(
            json!({"context": {"toolResultElision": false, "masking": {"keepRecentTurns": 4}}})
        ))["context"],
        json!({"masking": {"keepRecentTurns": 4, "enabled": false}})
    );
    // An explicit `enabled` wins over the legacy key.
    assert_eq!(
        accepted(with(
            json!({"context": {"toolResultElision": false, "masking": {"enabled": true}}})
        ))["context"],
        json!({"masking": {"enabled": true}})
    );
}

#[test]
fn retired_tool_result_elision_key_is_dropped() {
    assert_eq!(
        accepted(with(
            json!({"context": {"toolResultElision": {"maxChars": 10}}})
        ))["context"],
        json!({})
    );
    assert_eq!(
        accepted(with(json!({"context": {"toolResultElision": true}})))["context"],
        json!({})
    );
}

#[test]
fn skills_grammar() {
    assert_eq!(accepted(with(json!({"skills": []})))["skills"], json!([]));
    let descriptor = accepted(with(json!({
        "skills": [{"id": "a", "revision": "1", "extra": "dropped"}]
    })));
    assert_eq!(
        descriptor["skills"],
        json!([{"id": "a", "revision": "1"}]),
        "skills entry not normalized to {{id, revision}}"
    );
    // Entries missing a required member are malformed-present: they used to
    // be accepted with the whole `skills` field silently dropped.
    rejected(
        with(json!({"skills": [{"id": "a"}]})),
        "skills entry missing revision",
    );
    rejected(
        with(json!({"skills": [{"revision": "1"}]})),
        "skills entry missing id",
    );
    rejected(
        with(json!({"skills": [{"id": 1, "revision": "1"}]})),
        "skills entry non-string id",
    );
    rejected(
        with(json!({"skills": [{"id": "a", "revision": 1}]})),
        "skills entry non-string revision",
    );
    rejected(
        with(json!({"skills": [{"id": "a", "revision": null}]})),
        "skills entry explicit-null revision",
    );
    rejected(with(json!({"skills": ["a"]})), "skills entry non-record");
    // Non-array skills (including null and `{}`) are malformed-present.
    for bad in [json!("skill"), json!(5), json!({"id": "a"}), Value::Null] {
        rejected(
            with(json!({"skills": bad.clone()})),
            &format!("skills = {bad}"),
        );
    }
}

#[test]
fn custom_kind_grammar_matches_the_ts_regex() {
    // HOST_TASK_KIND = /^[a-z][a-z0-9_-]*(\.[a-z0-9_-]+)*$/ — later segments
    // may begin with digits or `_`, so these were wrongly rejected before.
    for kind in [
        "project.1",
        "project._worker",
        "project.worker",
        "project",
        "project.a-b",
        "project.x1_y-z",
        "a.b.c.d",
        CHAT_TURN_TASK_KIND,
    ] {
        assert!(is_valid_session_turn_kind(kind), "{kind} rejected");
        assert!(
            read(with(json!({"turnKind": kind}))).is_some(),
            "descriptor turnKind {kind} rejected"
        );
        assert_eq!(
            session_turn_kind(&accepted(with(json!({"turnKind": kind})))),
            kind
        );
    }
    for kind in [
        "",
        "Chat.x",
        "chat.other",
        "chat.turn.x",
        "agentkit.worker",
        ".project",
        "project.",
        "project..worker",
        "1project",
        "-project",
        "_project",
        "pro ject.a",
        "project.é",
        "project.a.b.",
    ] {
        assert!(!is_valid_session_turn_kind(kind), "{kind} accepted");
        rejected(with(json!({"turnKind": kind})), &format!("turnKind {kind}"));
    }
    for bad in [json!(5), json!(true), Value::Null] {
        rejected(
            with(json!({"turnKind": bad.clone()})),
            &format!("turnKind = {bad}"),
        );
    }
}

#[test]
fn turn_kind_defaults_to_chat_turn() {
    assert_eq!(session_turn_kind(&accepted(base())), CHAT_TURN_TASK_KIND);
    assert_eq!(
        session_turn_kind(&accepted(with(json!({"turnKind": "project.worker"})))),
        "project.worker"
    );
}

#[test]
fn content_selection_tracks_skills() {
    let descriptor = accepted(with(json!({"skills": [{"id": "a", "revision": "1"}]})));
    assert_eq!(
        session_content_selection(&descriptor),
        Some(json!({"template":null,"skills": [{"id": "a", "revision": "1"}]}))
    );
    assert_eq!(session_content_selection(&accepted(base())), None);
    assert_eq!(
        session_content_selection(&accepted(with(json!({"skills": []})))),
        None
    );
}

#[test]
fn malformed_present_never_silently_drops_the_field() {
    // Regression: each of these used to read as ABSENT (descriptor accepted,
    // field missing) instead of being rejected.
    for descriptor in [
        with(json!({"context": "final_answers"})),
        with(json!({"context": {"windowTokens": "4096"}})),
        with(json!({"context": {"windowTokens": 0}})),
        with(json!({"context": {"reserveTokens": "16384"}})),
        with(json!({"skills": "not-an-array"})),
        with(json!({"skills": [{"id": "a"}]})),
        with(json!({"workspace": "/project"})),
        with(json!({"workspace": {"root": "/project", "trusted": 1}})),
        with(json!({"profile": {"id": "p1", "digest": 7}})),
        with(json!({"providerId": 5})),
        with(json!({"reasoningEffort": "extreme"})),
        with(json!({"turnKind": "Project.worker"})),
    ] {
        rejected(descriptor.clone(), "silently-dropped malformed field");
    }
}
