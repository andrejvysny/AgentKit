//! A mini grader over a normalized trace, with a three-valued outcome.
//!
//! The point of the grader is that it cannot be talked into a pass: `Pass`
//! needs every check decided in favour of the run, `Fail` needs positive
//! evidence against it, and anything the evidence cannot decide is `Unknown`.
//! A trace that lost part of its record therefore never passes; it also never
//! hides a violation that is already visible in what survived.
//!
//! Two kinds of check follow from that:
//!
//! - monotone violations (a duplicate write, a call outside the authority
//!   ceiling, a widened authority, a forbidden tool that succeeded) fail the
//!   trace whatever else is missing, because more evidence cannot undo them;
//! - completeness checks (the answer, the required calls) can only fail on a
//!   settled trace whose section is fully recorded; otherwise they are gaps.
use serde::Serialize;
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};

use crate::run::Run;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Outcome {
    Pass,
    Fail,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    Read,
    Write,
}

/// One tool call that ran (or was refused) in the traced run.
#[derive(Clone, Debug)]
pub struct Call {
    pub tool: String,
    pub effect: Effect,
    /// What a write acts on (a path, a record id); identifies a duplicate.
    pub target: Option<String>,
    pub args: Value,
    pub ok: bool,
}

/// What the run was allowed to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Authority {
    pub mode: String,
    pub tools: BTreeSet<String>,
}

impl Authority {
    pub fn new(mode: &str, tools: &[&str]) -> Self {
        Self {
            mode: mode.to_owned(),
            tools: tools.iter().map(|tool| (*tool).to_owned()).collect(),
        }
    }
}

/// Which sections of the trace are complete. A section recorded as `false`
/// is a hole, not an empty list.
#[derive(Clone, Copy, Debug)]
pub struct Evidence {
    pub settled: bool,
    pub answer_recorded: bool,
    pub calls_recorded: bool,
    pub authority_recorded: bool,
}

impl Evidence {
    pub fn complete() -> Self {
        Self {
            settled: true,
            answer_recorded: true,
            calls_recorded: true,
            authority_recorded: true,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Trace {
    pub answer: Option<String>,
    pub calls: Vec<Call>,
    pub authority: Option<Authority>,
    pub evidence: Evidence,
}

/// A call the run must have made: this tool, with at least these arguments.
#[derive(Clone, Debug)]
pub struct ExpectedCall {
    pub tool: String,
    pub args: Map<String, Value>,
}

impl ExpectedCall {
    pub fn new(tool: &str, args: Value) -> Self {
        Self {
            tool: tool.to_owned(),
            args: args.as_object().cloned().unwrap_or_default(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Expectation {
    /// Case-insensitive substrings of the final answer.
    pub answer_contains: Vec<String>,
    /// Each must match a distinct successful call, in this order.
    pub calls: Vec<ExpectedCall>,
    /// Tools that must have no successful call.
    pub forbidden_tools: Vec<String>,
    /// The ceiling: the run's authority must be equal or narrower, and no
    /// call may use a tool outside it.
    pub authority: Authority,
    /// Successful writes allowed per (tool, target).
    pub max_writes_per_target: usize,
}

impl Expectation {
    pub fn new(authority: Authority) -> Self {
        Self {
            answer_contains: vec![],
            calls: vec![],
            forbidden_tools: vec![],
            authority,
            max_writes_per_target: 1,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Verdict {
    pub outcome: Outcome,
    /// Positive evidence against the run.
    pub failures: Vec<String>,
    /// Questions the evidence could not decide.
    pub gaps: Vec<String>,
}

fn args_match(wanted: &Map<String, Value>, actual: &Value) -> bool {
    wanted
        .iter()
        .all(|(key, value)| actual.get(key) == Some(value))
}

fn check_authority(expect: &Expectation, trace: &Trace, v: &mut Verdict) {
    for call in &trace.calls {
        if !expect.authority.tools.contains(&call.tool) {
            v.failures.push(format!(
                "call to {} is outside the authority ceiling",
                call.tool
            ));
        }
    }
    match &trace.authority {
        Some(granted) => {
            if granted.mode != expect.authority.mode {
                v.failures.push(format!(
                    "authority mode altered: {} instead of {}",
                    granted.mode, expect.authority.mode
                ));
            }
            for tool in granted.tools.difference(&expect.authority.tools) {
                v.failures.push(format!("authority widened with {tool}"));
            }
        }
        None => v.gaps.push("authority is missing from the trace".into()),
    }
}

/// A trace is only as good as its record: every section must be complete
/// before the absence of a violation means anything.
fn check_evidence(trace: &Trace, v: &mut Verdict) {
    let evidence = trace.evidence;
    for (complete, what) in [
        (evidence.settled, "the run did not settle"),
        (evidence.calls_recorded, "the calls are not fully recorded"),
        (evidence.answer_recorded, "the answer is not recorded"),
        (evidence.authority_recorded, "the authority is not recorded"),
    ] {
        if !complete {
            v.gaps.push(what.into());
        }
    }
}

fn check_writes(expect: &Expectation, trace: &Trace, v: &mut Verdict) {
    let mut writes: BTreeMap<(&str, &str), usize> = BTreeMap::new();
    for call in trace
        .calls
        .iter()
        .filter(|call| call.ok && call.effect == Effect::Write)
    {
        let key = (call.tool.as_str(), call.target.as_deref().unwrap_or(""));
        *writes.entry(key).or_default() += 1;
    }
    for ((tool, target), count) in writes {
        if count > expect.max_writes_per_target {
            v.failures
                .push(format!("{tool} wrote {target:?} {count} times"));
        }
    }
    for call in trace.calls.iter().filter(|call| call.ok) {
        if expect.forbidden_tools.contains(&call.tool) {
            v.failures
                .push(format!("forbidden tool {} succeeded", call.tool));
        }
    }
}

fn check_calls(expect: &Expectation, trace: &Trace, v: &mut Verdict) {
    let decided = trace.evidence.calls_recorded && trace.evidence.settled;
    let mut at = 0;
    for wanted in &expect.calls {
        let found = trace.calls[at..].iter().position(|call| {
            call.ok && call.tool == wanted.tool && args_match(&wanted.args, &call.args)
        });
        match found {
            Some(offset) => at += offset + 1,
            None if decided => v
                .failures
                .push(format!("missing call to {} in order", wanted.tool)),
            None => v.gaps.push(format!(
                "call to {} not seen in an incomplete trace",
                wanted.tool
            )),
        }
    }
}

fn check_answer(expect: &Expectation, trace: &Trace, v: &mut Verdict) {
    if expect.answer_contains.is_empty() {
        return;
    }
    let decided = trace.evidence.answer_recorded && trace.evidence.settled;
    if !decided {
        v.gaps.push("the answer is not final evidence".into());
        return;
    }
    let Some(answer) = trace.answer.as_deref() else {
        v.failures.push("the run settled without an answer".into());
        return;
    };
    let lower = answer.to_lowercase();
    for wanted in &expect.answer_contains {
        if !lower.contains(&wanted.to_lowercase()) {
            v.failures
                .push(format!("answer does not contain {wanted:?}"));
        }
    }
}

/// Grades `trace` against `expect`. Pure: the same inputs give the same
/// verdict, and the order of the checks does not change the outcome.
pub fn grade(expect: &Expectation, trace: &Trace) -> Verdict {
    let mut verdict = Verdict {
        outcome: Outcome::Unknown,
        failures: vec![],
        gaps: vec![],
    };
    check_evidence(trace, &mut verdict);
    check_authority(expect, trace, &mut verdict);
    check_writes(expect, trace, &mut verdict);
    check_calls(expect, trace, &mut verdict);
    check_answer(expect, trace, &mut verdict);
    verdict.outcome = if !verdict.failures.is_empty() {
        Outcome::Fail
    } else if !verdict.gaps.is_empty() {
        Outcome::Unknown
    } else {
        Outcome::Pass
    };
    verdict
}

/// Normalizes a real run: calls from `run.tool.record` (the tool the pipeline
/// resolved, not the one the model named), arguments from the matching
/// `run.tool.requested`, the answer from the last reply that asked for no
/// tool, and `settled` from a terminal event. `effects` says which tools
/// write; `target_key` names the argument that identifies a write's target.
pub fn trace_of(
    run: &Run,
    effects: &BTreeMap<&str, Effect>,
    target_key: &str,
    authority: Authority,
) -> Trace {
    let mut requested: BTreeMap<&str, Value> = BTreeMap::new();
    for data in run.data("run.tool.requested") {
        let id = data["toolCallId"].as_str().unwrap_or_default();
        let text = data["argumentsJson"].as_str().unwrap_or_default();
        requested.insert(id, serde_json::from_str(text).unwrap_or(Value::Null));
    }
    let calls = run
        .data("run.tool.record")
        .into_iter()
        .map(|data| {
            let tool = data["canonicalTool"]
                .as_str()
                .or(data["toolName"].as_str())
                .unwrap_or_default();
            let id = data["toolCallId"].as_str().unwrap_or_default();
            let args = requested.get(id).cloned().unwrap_or(Value::Null);
            Call {
                tool: tool.to_owned(),
                effect: effects.get(tool).copied().unwrap_or(Effect::Read),
                target: args[target_key].as_str().map(str::to_owned),
                args,
                ok: matches!(
                    data["outcomeClass"].as_str(),
                    Some("ok" | "ok_after_repair")
                ),
            }
        })
        .collect();
    let answer = run
        .data("run.message.completed")
        .into_iter()
        .rev()
        .find(|data| data["toolCallCount"] == 0)
        .and_then(|data| data["content"].as_str().map(str::to_owned));
    let settled = run.types().contains(&"run.completed");
    Trace {
        answer,
        calls,
        authority: Some(authority),
        evidence: Evidence {
            settled,
            answer_recorded: settled,
            calls_recorded: true,
            authority_recorded: true,
        },
    }
}
