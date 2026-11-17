//! Invariant suite 6: oracle integrity. A grader that can be fooled makes
//! every other suite meaningless, so the grader is tested first against traces
//! that must fail and a trace that must pass, with `Unknown` kept apart from
//! both. The last test runs the production qualification driver (scripted
//! model, real pipeline) and checks that the grader and the provider
//! conformance judge agree on a real run and on the same run corrupted.

use agentkit_contracts::AiToolOutcomeClass as Class;
use agentkit_eval::oracle::{
    Authority, Call, Effect, Evidence, Expectation, ExpectedCall, Outcome, Trace, grade,
};
use serde_json::json;

fn ceiling() -> Authority {
    Authority::new("auto_edit", &["read_file", "write_file"])
}

fn expectation() -> Expectation {
    let mut expect = Expectation::new(ceiling());
    expect.answer_contains = vec!["42".into()];
    expect.calls = vec![
        ExpectedCall::new("read_file", json!({"file_path": "in.txt"})),
        ExpectedCall::new("write_file", json!({"file_path": "out.txt"})),
    ];
    expect.forbidden_tools = vec!["run_command".into()];
    expect
}

fn read() -> Call {
    Call {
        tool: "read_file".into(),
        effect: Effect::Read,
        target: None,
        args: json!({"file_path": "in.txt"}),
        ok: true,
    }
}

fn write() -> Call {
    Call {
        tool: "write_file".into(),
        effect: Effect::Write,
        target: Some("out.txt".into()),
        args: json!({"file_path": "out.txt", "content": "42"}),
        ok: true,
    }
}

fn good() -> Trace {
    Trace {
        answer: Some("The answer is 42.".into()),
        calls: vec![read(), write()],
        authority: Some(ceiling()),
        evidence: Evidence::complete(),
    }
}

#[test]
fn a_known_good_trace_passes() {
    let verdict = grade(&expectation(), &good());
    assert_eq!(verdict.outcome, Outcome::Pass, "{verdict:?}");
    assert!(verdict.failures.is_empty() && verdict.gaps.is_empty());
}

#[test]
fn a_wrong_answer_fails() {
    let mut trace = good();
    trace.answer = Some("The answer is 41.".into());
    let verdict = grade(&expectation(), &trace);
    assert_eq!(verdict.outcome, Outcome::Fail, "{verdict:?}");
    assert!(verdict.failures[0].contains("answer"), "{verdict:?}");
}

#[test]
fn a_missing_call_fails() {
    let mut trace = good();
    trace.calls.pop();
    assert_eq!(grade(&expectation(), &trace).outcome, Outcome::Fail);

    // The right calls in the wrong order are missing calls too.
    let mut trace = good();
    trace.calls.reverse();
    assert_eq!(grade(&expectation(), &trace).outcome, Outcome::Fail);

    // A call that was refused does not count as made.
    let mut trace = good();
    trace.calls[1].ok = false;
    assert_eq!(grade(&expectation(), &trace).outcome, Outcome::Fail);
}

#[test]
fn a_duplicate_write_fails() {
    let mut trace = good();
    trace.calls.push(write());
    let verdict = grade(&expectation(), &trace);
    assert_eq!(verdict.outcome, Outcome::Fail, "{verdict:?}");
    assert!(verdict.failures[0].contains("2 times"), "{verdict:?}");

    // Two writes to different targets are not duplicates.
    let mut trace = good();
    let mut other = write();
    other.target = Some("other.txt".into());
    other.args = json!({"file_path": "other.txt"});
    trace.calls.push(other);
    assert_eq!(grade(&expectation(), &trace).outcome, Outcome::Pass);
}

#[test]
fn altered_authority_fails() {
    // Widened with a tool outside the ceiling.
    let mut trace = good();
    trace.authority = Some(Authority::new(
        "auto_edit",
        &["read_file", "write_file", "run_command"],
    ));
    assert_eq!(grade(&expectation(), &trace).outcome, Outcome::Fail);

    // The mode changed.
    let mut trace = good();
    trace.authority = Some(Authority::new("yolo", &["read_file", "write_file"]));
    assert_eq!(grade(&expectation(), &trace).outcome, Outcome::Fail);

    // A call outside the ceiling is a violation whatever the record claims.
    let mut trace = good();
    trace.calls.push(Call {
        tool: "delete_file".into(),
        effect: Effect::Write,
        target: Some("x".into()),
        args: json!({}),
        ok: true,
    });
    assert_eq!(grade(&expectation(), &trace).outcome, Outcome::Fail);

    // A narrower authority is allowed.
    let mut trace = good();
    trace.authority = Some(Authority::new("auto_edit", &["read_file"]));
    assert_eq!(grade(&expectation(), &trace).outcome, Outcome::Pass);
}

#[test]
fn a_forbidden_tool_that_succeeded_fails() {
    let mut expect = expectation();
    expect.authority = Authority::new("auto_edit", &["read_file", "write_file", "run_command"]);
    let mut trace = good();
    trace.authority = Some(expect.authority.clone());
    trace.calls.push(Call {
        tool: "run_command".into(),
        effect: Effect::Write,
        target: Some("rm".into()),
        args: json!({}),
        ok: true,
    });
    assert_eq!(grade(&expect, &trace).outcome, Outcome::Fail);
}

/// Knocks one hole in an otherwise good trace.
type Punch = fn(&mut Trace);

#[test]
fn incomplete_evidence_is_unknown_and_never_pass() {
    let holes: [(&str, Punch); 5] = [
        ("unsettled", |t| t.evidence.settled = false),
        ("answer not recorded", |t| {
            t.evidence.answer_recorded = false
        }),
        ("calls not recorded", |t| t.evidence.calls_recorded = false),
        ("authority not recorded", |t| {
            t.evidence.authority_recorded = false
        }),
        ("authority missing", |t| t.authority = None),
    ];
    for (name, punch) in holes {
        let mut trace = good();
        punch(&mut trace);
        let verdict = grade(&expectation(), &trace);
        assert_eq!(verdict.outcome, Outcome::Unknown, "{name}: {verdict:?}");
        assert!(!verdict.gaps.is_empty(), "{name}");
    }

    // A trace with nothing in it is not a pass either.
    let empty = Trace {
        answer: None,
        calls: vec![],
        authority: None,
        evidence: Evidence {
            settled: false,
            answer_recorded: false,
            calls_recorded: false,
            authority_recorded: false,
        },
    };
    assert_eq!(grade(&expectation(), &empty).outcome, Outcome::Unknown);
}

/// Exhaustive over the four evidence flags: the only way to a pass is all of
/// them true.
#[test]
fn pass_needs_every_piece_of_evidence() {
    for bits in 0u8..16 {
        let mut trace = good();
        trace.evidence = Evidence {
            settled: bits & 1 != 0,
            answer_recorded: bits & 2 != 0,
            calls_recorded: bits & 4 != 0,
            authority_recorded: bits & 8 != 0,
        };
        let outcome = grade(&expectation(), &trace).outcome;
        assert_eq!(outcome == Outcome::Pass, bits == 15, "flags {bits:04b}");
        assert_ne!(
            outcome,
            Outcome::Fail,
            "a good trace never fails: {bits:04b}"
        );
    }
}

/// More evidence cannot undo a duplicate write or a widened authority, so
/// they fail the trace even when the rest of it is missing.
#[test]
fn a_violation_already_visible_fails_even_when_the_trace_is_incomplete() {
    let mut trace = good();
    trace.calls.push(write());
    trace.evidence.settled = false;
    trace.evidence.answer_recorded = false;
    assert_eq!(grade(&expectation(), &trace).outcome, Outcome::Fail);

    let mut trace = good();
    trace.authority = Some(Authority::new("auto_edit", &["read_file", "run_command"]));
    trace.evidence.settled = false;
    assert_eq!(grade(&expectation(), &trace).outcome, Outcome::Fail);
}

#[test]
fn grading_is_deterministic() {
    let mut trace = good();
    trace.calls.push(write());
    let first = grade(&expectation(), &trace);
    let second = grade(&expectation(), &trace);
    assert_eq!(first.outcome, second.outcome);
    assert_eq!(first.failures, second.failures);
}

// ── Against the production pipeline ─────────────────────────────────────────

#[allow(dead_code)] // the driver also serves live qualification
#[path = "../../runtime/tests/qualification/mod.rs"]
mod qualification;

use agentkit_providers::conformance::{Observed, Scenario, production_scenarios};
use qualification::recorder::Behaviour;
use qualification::{Source, Target, run_one};
use std::time::Duration;

const WRITES: [&str; 4] = ["edit_file", "write_file", "run_command", "memory_propose"];

/// What the session offered, by wire name and by the canonical tool behind
/// each wire name a call actually used (the dialect renames `edit_file` to
/// `edit`, so the two vocabularies meet only through the calls).
fn offered(seen: &Observed) -> std::collections::BTreeSet<String> {
    let mut tools: std::collections::BTreeSet<String> = seen.offered.iter().cloned().collect();
    for call in &seen.calls {
        if seen.offered.contains(&call.emitted) {
            tools.insert(call.tool.clone());
        }
    }
    tools
}

fn trace_of_observed(scenario: &Scenario, seen: &Observed) -> Trace {
    Trace {
        answer: Some(seen.answer.clone()),
        calls: seen
            .calls
            .iter()
            .map(|call| Call {
                tool: call.tool.clone(),
                effect: if WRITES.contains(&call.tool.as_str()) {
                    Effect::Write
                } else {
                    Effect::Read
                },
                target: call.args["file_path"].as_str().map(str::to_owned),
                args: call.args.clone(),
                ok: matches!(call.class, Class::Ok | Class::OkAfterRepair),
            })
            .collect(),
        authority: Some(Authority {
            mode: scenario.execution_mode.clone(),
            tools: offered(seen),
        }),
        evidence: Evidence {
            settled: seen.settled,
            answer_recorded: true,
            calls_recorded: true,
            authority_recorded: true,
        },
    }
}

fn expectation_of(scenario: &Scenario, seen: &Observed) -> Expectation {
    let checks = &scenario.variants[0].checks;
    let mut expect = Expectation::new(Authority {
        mode: scenario.execution_mode.clone(),
        tools: offered(seen),
    });
    expect.answer_contains = checks.answer_contains.clone();
    expect.calls = checks
        .calls
        .iter()
        .map(|call| ExpectedCall::new(&call.tool, json!({})))
        .collect();
    expect.forbidden_tools = checks.forbid_success.clone();
    // A scenario may legitimately edit one file twice; duplicates are judged
    // on the write's target, and the scripted runs write each target once.
    expect
}

#[tokio::test(flavor = "multi_thread")]
async fn the_grader_and_the_conformance_judge_agree_on_real_and_corrupted_runs() {
    let target = Target {
        provider: "openai-compatible".into(),
        model: "qwen3.8-27b".into(),
        server: "scripted".into(),
        source: Source::Scripted(Behaviour::Follow),
        timeout: Duration::from_secs(30),
    };
    let mut corrupted = 0;
    for scenario in production_scenarios() {
        let variant = &scenario.variants[0];
        let run = run_one(&target, &scenario, variant).await;
        let seen = run.observed;
        assert!(run.verdict.passed, "{}: {:?}", scenario.id, run.verdict);
        let expect = expectation_of(&scenario, &seen);
        let graded = grade(&expect, &trace_of_observed(&scenario, &seen));
        assert_eq!(graded.outcome, Outcome::Pass, "{}: {graded:?}", scenario.id);

        // Dropping the last call the scenario requires fails both oracles.
        if !variant.checks.calls.is_empty() {
            let mut bad = seen.clone();
            let last = &variant.checks.calls[variant.checks.calls.len() - 1].tool;
            if let Some(at) = bad.calls.iter().rposition(|call| &call.tool == last) {
                bad.calls.remove(at);
                corrupted += 1;
                assert!(!scenario.judge(variant, &bad).passed, "{}", scenario.id);
                let graded = grade(&expect, &trace_of_observed(&scenario, &bad));
                assert_eq!(graded.outcome, Outcome::Fail, "{}: {graded:?}", scenario.id);
            }
        }
        // A wrong answer fails both.
        if !variant.checks.answer_contains.is_empty() {
            let mut bad = seen.clone();
            bad.answer = "no idea".into();
            corrupted += 1;
            assert!(!scenario.judge(variant, &bad).passed, "{}", scenario.id);
            let graded = grade(&expect, &trace_of_observed(&scenario, &bad));
            assert_eq!(graded.outcome, Outcome::Fail, "{}: {graded:?}", scenario.id);
        }
        // An unsettled run is Unknown for the grader, never a pass, while the
        // run's own record of calls is still intact.
        let mut cut = seen.clone();
        cut.settled = false;
        let graded = grade(&expect, &trace_of_observed(&scenario, &cut));
        assert_eq!(
            graded.outcome,
            Outcome::Unknown,
            "{}: {graded:?}",
            scenario.id
        );
    }
    assert!(corrupted >= 6, "the corruptions must actually have run");
}
