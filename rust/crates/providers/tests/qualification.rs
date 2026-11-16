//! The scenario and record library of R21: judging a run from the real final
//! state, the version tuple a qualification is valid for, and the standing
//! scenario set's shape.
use agentkit_contracts::AiToolOutcomeClass as Class;
use agentkit_providers::conformance::{
    Observed, ObservedCall, ObservedMemory, QualificationKey, QualificationRecord, Scenario,
    ScenarioResult, Thresholds, digest_json, production_scenarios,
};
use serde_json::json;

fn scenario(id: &str) -> Scenario {
    production_scenarios()
        .into_iter()
        .find(|s| s.id == id)
        .expect("scenario")
}

fn call(tool: &str, args: serde_json::Value, class: Class) -> ObservedCall {
    ObservedCall {
        tool: tool.into(),
        emitted: tool.into(),
        args,
        class,
    }
}

fn key(toolsets: &[&str]) -> QualificationKey {
    QualificationKey {
        model: "qwen3.8-27b".into(),
        provider: "openai-compatible".into(),
        server: "litellm".into(),
        profile_id: "qwen3.8-27b".into(),
        profile_version: "1".into(),
        profile_digest: digest_json(&json!({"p": 1})),
        schema_digest: digest_json(&json!([{"name": "read_file"}])),
        prompt_digest: digest_json(&json!("prompt")),
        toolsets: toolsets.iter().map(|t| (*t).to_owned()).collect(),
    }
}

#[test]
fn standing_set_has_six_scenarios_in_both_languages() {
    let set = production_scenarios();
    assert!(set.len() >= 6);
    for scenario in &set {
        let langs: Vec<&str> = scenario.variants.iter().map(|v| v.lang.as_str()).collect();
        assert_eq!(langs, ["en", "sk"], "{}", scenario.id);
    }
    let ids: Vec<&str> = set.iter().map(|s| s.id.as_str()).collect();
    for want in [
        "read_file",
        "edit_file",
        "search",
        "multi_step_edit",
        "refusal",
        "memory_propose",
    ] {
        assert!(ids.contains(&want), "{want}");
    }
}

#[test]
fn judge_reads_final_state_not_just_that_a_tool_was_called() {
    let edit = scenario("edit_file");
    let variant = &edit.variants[0];
    let mut seen = Observed {
        offered: vec!["read_file".into(), "edit_file".into()],
        calls: vec![
            call("read_file", json!({"file_path": "config.toml"}), Class::Ok),
            call("edit_file", json!({"file_path": "config.toml"}), Class::Ok),
        ],
        files: edit.files.clone(),
        settled: true,
        ..Observed::default()
    };
    let verdict = edit.judge(variant, &seen);
    assert!(!verdict.passed, "edit called but file unchanged must fail");
    seen.files.insert(
        "config.toml".into(),
        "name = \"demo\"\ntimeout = 60\nretries = 3\n".into(),
    );
    assert!(edit.judge(variant, &seen).passed);
}

#[test]
fn judge_checks_order_unoffered_tools_and_forbidden_successes() {
    let edit = scenario("edit_file");
    let variant = &edit.variants[0];
    let mut seen = Observed {
        offered: vec!["read_file".into(), "edit_file".into()],
        calls: vec![
            call("edit_file", json!({"file_path": "config.toml"}), Class::Ok),
            call("read_file", json!({"file_path": "config.toml"}), Class::Ok),
        ],
        files: edit.files.clone(),
        settled: true,
        ..Observed::default()
    };
    seen.files.insert(
        "config.toml".into(),
        "name = \"demo\"\ntimeout = 60\nretries = 3\n".into(),
    );
    let verdict = edit.judge(variant, &seen);
    assert!(
        verdict.failures.iter().any(|f| f.contains("in-order")),
        "{verdict:?}"
    );

    let refusal = scenario("refusal");
    let seen = Observed {
        offered: vec!["write_file".into()],
        calls: vec![
            call("write_file", json!({}), Class::Ok),
            call("shell", json!({}), Class::UnknownTool),
        ],
        files: [("secrets.env".to_owned(), String::new())].into(),
        settled: true,
        ..Observed::default()
    };
    let verdict = refusal.judge(&refusal.variants[1], &seen);
    assert!(!verdict.passed);
    assert_eq!(verdict.invalid_calls, 1);
    assert!(verdict.failures.iter().any(|f| f.contains("must not")));
    assert!(verdict.failures.iter().any(|f| f.contains("not offered")));
}

#[test]
fn memory_state_is_judged_on_the_record_not_the_call() {
    let memory = scenario("memory_propose");
    let variant = &memory.variants[0];
    let mut seen = Observed {
        offered: vec!["memory_propose".into()],
        calls: vec![call(
            "memory_propose",
            json!({"text": "I prefer documents in English."}),
            Class::Ok,
        )],
        settled: true,
        ..Observed::default()
    };
    assert!(!memory.judge(variant, &seen).passed);
    seen.memory.push(ObservedMemory {
        status: "active".into(),
        text: "I prefer documents in English.".into(),
    });
    assert!(memory.judge(variant, &seen).passed);
}

fn result(toolset: &str, passed: bool, calls: u32, invalid: u32) -> ScenarioResult {
    ScenarioResult {
        scenario: "s".into(),
        toolset: toolset.into(),
        lang: "en".into(),
        passed,
        calls,
        invalid_calls: invalid,
        failures: vec![],
    }
}

#[test]
fn record_qualifies_per_toolset_and_only_for_its_exact_tuple() {
    let results: Vec<ScenarioResult> = (0..10)
        .map(|_| result("files", true, 2, 0))
        .chain((0..4).map(|n| result("memory", n != 0, 1, 0)))
        .collect();
    let record = QualificationRecord::build(
        key(&["memory", "files"]),
        Thresholds::default(),
        results,
        "2026-01-01T00:00:00Z".into(),
        "abc123".into(),
    );
    assert_eq!(record.qualified_toolsets, ["files"]);
    assert_eq!(record.key.toolsets, ["files", "memory"]);

    let current = key(&["files", "memory"]);
    assert!(record.qualifies(&current, "files"));
    assert!(!record.qualifies(&current, "memory"));
    for changed in [
        QualificationKey {
            model: "other".into(),
            ..current.clone()
        },
        QualificationKey {
            schema_digest: digest_json(&json!([{"name": "read_file", "description": "x"}])),
            ..current.clone()
        },
        QualificationKey {
            prompt_digest: digest_json(&json!("other prompt")),
            ..current.clone()
        },
        QualificationKey {
            profile_digest: digest_json(&json!({"p": 2})),
            ..current.clone()
        },
        QualificationKey {
            server: "vllm".into(),
            ..current.clone()
        },
        key(&["files"]),
    ] {
        assert!(!record.qualifies(&changed, "files"), "{changed:?}");
    }
}

#[test]
fn record_json_carries_the_versioned_tuple() {
    let record = QualificationRecord::build(
        key(&["files"]),
        Thresholds::default(),
        vec![result("files", true, 1, 0)],
        "2026-01-01T00:00:00Z".into(),
        "abc123".into(),
    );
    let value: serde_json::Value = serde_json::from_str(&record.to_json()).unwrap();
    for field in [
        "record_version",
        "model",
        "provider",
        "server",
        "profile_id",
        "profile_version",
        "schema_digest",
        "prompt_digest",
        "toolsets",
        "scenarios",
        "timestamp",
        "agentkit_commit",
    ] {
        assert!(value.get(field).is_some(), "{field}");
    }
    assert_eq!(value["record_version"], "agentkit.tool-use-qualification/1");
}
