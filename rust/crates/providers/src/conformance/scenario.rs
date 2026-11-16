//! Production-pipeline scenarios (R21): what to ask, what the scripted model
//! does by default, and how the final state is judged. A scenario names real
//! tools only; schemas, descriptions, profile, dialect and the run loop come
//! from the host driver that runs it (see `docs/qualification/`).
use super::check::{args_match, is_invalid};
use super::task::ExpectedCall;
use agentkit_contracts::AiToolOutcomeClass as Class;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;

/// One scripted model call (default test runs only).
#[derive(Clone, Debug)]
pub struct ScriptedCall {
    pub tool: String,
    pub args: Value,
}

/// What the scripted model does: calls per turn, then the final answer.
#[derive(Clone, Debug, Default)]
pub struct Script {
    pub turns: Vec<Vec<ScriptedCall>>,
    pub answer: String,
}

#[derive(Clone, Debug)]
pub enum StateCheck {
    FileEquals {
        path: String,
        value: String,
    },
    FileContains {
        path: String,
        text: String,
    },
    FileAbsent {
        path: String,
    },
    /// The seeded content is still there, byte for byte.
    FileUnchanged {
        path: String,
    },
    /// A memory record with this status whose text contains `text`.
    MemoryHas {
        status: String,
        text: String,
    },
    /// No memory record at all.
    MemoryEmpty,
}

#[derive(Clone, Debug, Default)]
pub struct Checks {
    /// Each must match a distinct successful call, in this order.
    pub calls: Vec<ExpectedCall>,
    /// Tools that must have no successful call.
    pub forbid_success: Vec<String>,
    pub state: Vec<StateCheck>,
    /// Case-insensitive substrings of the final answer.
    pub answer_contains: Vec<String>,
}

/// One language of a scenario.
#[derive(Clone, Debug)]
pub struct Variant {
    /// `en` or `sk`.
    pub lang: String,
    pub prompt: String,
    pub script: Script,
    pub checks: Checks,
}

#[derive(Clone, Debug)]
pub struct Scenario {
    pub id: String,
    pub toolset: String,
    /// The session's execution mode (`ask`, `auto_edit`, `read_only`, ...).
    pub execution_mode: String,
    /// Initial workspace files, relative path to text.
    pub files: BTreeMap<String, String>,
    pub variants: Vec<Variant>,
}

/// A call the run loop recorded, with its outcome class.
#[derive(Clone, Debug)]
pub struct ObservedCall {
    /// The canonical tool that ran.
    pub tool: String,
    /// The name the model wrote, in the session's dialect (`tool_call` for a
    /// deferred tool reached through the meta tool).
    pub emitted: String,
    pub args: Value,
    pub class: Class,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservedMemory {
    pub status: String,
    pub text: String,
}

/// The state after a run, read from the real ports.
#[derive(Clone, Debug, Default)]
pub struct Observed {
    /// Wire names the model was offered (first request's `tools`).
    pub offered: Vec<String>,
    pub calls: Vec<ObservedCall>,
    /// Files in the workspace now; a missing entry is an absent file.
    pub files: BTreeMap<String, String>,
    pub memory: Vec<ObservedMemory>,
    pub answer: String,
    /// The turn settled with a final answer (not stuck, failed or cut off).
    pub settled: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Verdict {
    pub passed: bool,
    pub calls: u32,
    pub invalid_calls: u32,
    pub failures: Vec<String>,
}

fn succeeded(class: Class) -> bool {
    matches!(class, Class::Ok | Class::OkAfterRepair)
}

fn state_failure(check: &StateCheck, scenario: &Scenario, seen: &Observed) -> Option<String> {
    let file = |path: &String| seen.files.get(path);
    let bad = match check {
        StateCheck::FileEquals { path, value } => file(path) != Some(value),
        StateCheck::FileContains { path, text } => !file(path).is_some_and(|c| c.contains(text)),
        StateCheck::FileAbsent { path } => file(path).is_some(),
        StateCheck::FileUnchanged { path } => file(path) != scenario.files.get(path),
        StateCheck::MemoryHas { status, text } => !seen
            .memory
            .iter()
            .any(|r| r.status == *status && r.text.contains(text.as_str())),
        StateCheck::MemoryEmpty => !seen.memory.is_empty(),
    };
    bad.then(|| format!("state check failed: {check:?}"))
}

impl Scenario {
    /// Judges one run: the final state, the call sequence and the answer.
    pub fn judge(&self, variant: &Variant, seen: &Observed) -> Verdict {
        let mut failures = Vec::new();
        if !seen.settled {
            failures.push("turn did not settle with a final answer".to_owned());
        }
        for call in &seen.calls {
            if !seen.offered.is_empty() && !seen.offered.contains(&call.emitted) {
                failures.push(format!("called '{}', which was not offered", call.emitted));
            }
        }
        let wins: Vec<&ObservedCall> = seen.calls.iter().filter(|c| succeeded(c.class)).collect();
        let mut from = 0;
        for want in &variant.checks.calls {
            let hit = wins[from..].iter().position(|c| {
                c.tool == want.tool && c.args.as_object().is_some_and(|a| args_match(want, a))
            });
            match hit {
                Some(i) => from += i + 1,
                None => failures.push(format!("no in-order successful call '{}'", want.tool)),
            }
        }
        for tool in &variant.checks.forbid_success {
            if wins.iter().any(|c| &c.tool == tool) {
                failures.push(format!("'{tool}' succeeded but must not"));
            }
        }
        failures.extend(
            variant
                .checks
                .state
                .iter()
                .filter_map(|check| state_failure(check, self, seen)),
        );
        let answer = seen.answer.to_lowercase();
        for needle in &variant.checks.answer_contains {
            if !answer.contains(&needle.to_lowercase()) {
                failures.push(format!("answer lacks '{needle}'"));
            }
        }
        let count = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
        Verdict {
            passed: failures.is_empty(),
            calls: count(seen.calls.len()),
            invalid_calls: count(seen.calls.iter().filter(|c| is_invalid(c.class)).count()),
            failures,
        }
    }
}
