//! Fake workspace, call classification and the automatic checkers.
use super::task::{Effect, ExpectedCall, Predicate, Task, ToolSpec};
use agentkit_contracts::AiToolOutcomeClass as Class;
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashMap, HashSet};

/// One classified call. Mirrors the T0.1 record's class and repair rules
/// (`agentkit-core` keeps its own recorder crate-private).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallRecord {
    pub step: u32,
    pub tool: String,
    pub class: Class,
}

/// The snake_case wire name of a class, as the T0.1 record writes it.
pub fn class_name(class: Class) -> String {
    serde_json::to_value(class)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// The three invalid classes plus `unknown_tool` (the qualification numerator).
pub fn is_invalid(class: Class) -> bool {
    matches!(
        class,
        Class::InvalidJson | Class::InvalidStructure | Class::InvalidValue | Class::UnknownTool
    )
}

type Executed = Result<(Map<String, Value>, String), (Class, String)>;

pub(crate) struct Sandbox {
    tools: Vec<ToolSpec>,
    pub entries: BTreeMap<String, String>,
    pub records: Vec<CallRecord>,
    /// Arguments of every successful call, in order.
    pub successes: Vec<(String, Map<String, Value>)>,
    failures: HashMap<String, u32>,
    failed_calls: HashSet<(String, String)>,
}

impl Sandbox {
    pub fn new(task: &Task) -> Self {
        Self {
            tools: task.allowed_tools.clone(),
            entries: task.workspace.clone(),
            records: Vec::new(),
            successes: Vec::new(),
            failures: HashMap::new(),
            failed_calls: HashSet::new(),
        }
    }

    /// Runs one call, records its class and returns the tool message text.
    pub fn call(&mut self, step: u32, name: &str, raw_args: &str) -> String {
        let outcome = self.execute(name, raw_args);
        let key = (name.to_owned(), raw_args.to_owned());
        let prior = self.failures.get(name).copied().unwrap_or(0);
        let (class, text) = match outcome {
            Ok((args, text)) => {
                self.successes.push((name.to_owned(), args));
                (
                    if prior > 0 {
                        Class::OkAfterRepair
                    } else {
                        Class::Ok
                    },
                    text,
                )
            }
            Err((class, text)) => {
                let repeated = self.failed_calls.contains(&key);
                *self.failures.entry(name.to_owned()).or_default() += 1;
                self.failed_calls.insert(key);
                (if repeated { Class::RepeatIdentical } else { class }, text)
            }
        };
        self.records.push(CallRecord {
            step,
            tool: name.to_owned(),
            class,
        });
        text
    }

    pub fn record_announced(&mut self, step: u32) {
        self.records.push(CallRecord {
            step,
            tool: String::new(),
            class: Class::AnnouncedNoCall,
        });
    }

    fn execute(&mut self, name: &str, raw: &str) -> Executed {
        let Some(spec) = self.tools.iter().find(|t| t.name == name).cloned() else {
            return Err((Class::UnknownTool, format!("unknown tool '{name}'")));
        };
        let args = parse_args(&spec, raw)?;
        let text = |map: &Map<String, Value>, key: &str| {
            map.get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        let reply = match &spec.effect {
            Effect::Reply { text } => text.clone(),
            Effect::Read { key_arg } => {
                let key = text(&args, key_arg);
                self.entries
                    .get(&key)
                    .cloned()
                    .ok_or((Class::Precondition, format!("'{key}' does not exist")))?
            }
            Effect::Write { key_arg, value_arg } => {
                self.entries
                    .insert(text(&args, key_arg), text(&args, value_arg));
                "ok".into()
            }
            Effect::Append { key_arg, value_arg } => {
                self.entries
                    .entry(text(&args, key_arg))
                    .or_default()
                    .push_str(&text(&args, value_arg));
                "ok".into()
            }
            Effect::Log { key, value_arg } => {
                let line = format!("{}\n", text(&args, value_arg));
                self.entries.entry(key.clone()).or_default().push_str(&line);
                "ok".into()
            }
            Effect::Remove { key_arg } => {
                let key = text(&args, key_arg);
                self.entries
                    .remove(&key)
                    .map(|_| "ok".to_owned())
                    .ok_or((Class::Precondition, format!("'{key}' does not exist")))?
            }
        };
        Ok((args, reply))
    }
}

fn parse_args(spec: &ToolSpec, raw: &str) -> Result<Map<String, Value>, (Class, String)> {
    let raw = if raw.trim().is_empty() { "{}" } else { raw };
    let value: Value = serde_json::from_str(raw)
        .map_err(|e| (Class::InvalidJson, format!("arguments are not JSON: {e}")))?;
    let Value::Object(map) = value else {
        return Err((
            Class::InvalidStructure,
            "arguments must be an object".into(),
        ));
    };
    if let Some(missing) = spec.required.iter().find(|k| !map.contains_key(*k)) {
        return Err((
            Class::InvalidStructure,
            format!("missing required argument '{missing}'"),
        ));
    }
    let known = |k: &String| spec.required.contains(k) || spec.optional.contains(k);
    if let Some(extra) = map.keys().find(|k| !known(k)) {
        return Err((
            Class::InvalidStructure,
            format!("unexpected argument '{extra}'"),
        ));
    }
    if let Some((key, _)) = map.iter().find(|(_, v)| !v.is_string()) {
        return Err((
            Class::InvalidValue,
            format!("argument '{key}' must be a string"),
        ));
    }
    Ok(map)
}

fn args_match(expected: &ExpectedCall, actual: &Map<String, Value>) -> bool {
    expected
        .args
        .iter()
        .all(|(key, want)| match actual.get(key) {
            Some(_) if want == "*" => true,
            Some(got) if want.as_str().is_some_and(|w| w.starts_with("contains:")) => got
                .as_str()
                .zip(want.as_str())
                .is_some_and(|(g, w)| g.contains(&w["contains:".len()..])),
            Some(got) => got == want,
            None => false,
        })
}

/// Expected-call matcher: every expected call needs its own successful call.
pub(crate) fn expected_calls_met(
    expected: &[ExpectedCall],
    successes: &[(String, Map<String, Value>)],
) -> Result<(), String> {
    let mut used = vec![false; successes.len()];
    for want in expected {
        let hit = successes
            .iter()
            .enumerate()
            .position(|(i, (tool, args))| !used[i] && *tool == want.tool && args_match(want, args));
        match hit {
            Some(i) => used[i] = true,
            None => return Err(format!("no successful call matches '{}'", want.tool)),
        }
    }
    Ok(())
}

/// State predicate over the fake workspace.
pub(crate) fn predicates_hold(
    predicates: &[Predicate],
    entries: &BTreeMap<String, String>,
) -> Result<(), String> {
    for predicate in predicates {
        let ok = match predicate {
            Predicate::EntryExists { key } => entries.contains_key(key),
            Predicate::EntryAbsent { key } => !entries.contains_key(key),
            Predicate::EntryEquals { key, value } => entries.get(key) == Some(value),
            Predicate::EntryContains { key, text } => {
                entries.get(key).is_some_and(|v| v.contains(text.as_str()))
            }
        };
        if !ok {
            return Err(format!("state predicate failed: {predicate:?}"));
        }
    }
    Ok(())
}

pub(crate) fn answer_holds(needles: &[String], answer: &str) -> Result<(), String> {
    let lower = answer.to_lowercase();
    match needles.iter().find(|n| !lower.contains(&n.to_lowercase())) {
        Some(missing) => Err(format!("answer lacks '{missing}'")),
        None => Ok(()),
    }
}

/// All three checkers; the first failure is the reason.
pub(crate) fn check_task(task: &Task, sandbox: &Sandbox, answer: &str) -> Result<(), String> {
    expected_calls_met(&task.expect.calls, &sandbox.successes)?;
    predicates_hold(&task.expect.state, &sandbox.entries)?;
    answer_holds(&task.expect.answer_contains, answer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec(name: &str, effect: Effect) -> ToolSpec {
        ToolSpec {
            name: name.into(),
            required: vec!["path".into()],
            optional: vec!["content".into()],
            effect,
        }
    }
    fn sandbox() -> Sandbox {
        Sandbox {
            tools: vec![
                spec(
                    "read",
                    Effect::Read {
                        key_arg: "path".into(),
                    },
                ),
                spec(
                    "write",
                    Effect::Write {
                        key_arg: "path".into(),
                        value_arg: "content".into(),
                    },
                ),
            ],
            entries: BTreeMap::new(),
            records: Vec::new(),
            successes: Vec::new(),
            failures: HashMap::new(),
            failed_calls: HashSet::new(),
        }
    }
    fn classes(s: &Sandbox) -> Vec<Class> {
        s.records.iter().map(|r| r.class).collect()
    }

    #[test]
    fn classifies_each_failure_kind() {
        let mut s = sandbox();
        s.call(1, "nope", "{}");
        s.call(1, "read", "{not json");
        s.call(1, "read", "[]");
        s.call(1, "read", r#"{"path":1}"#);
        s.call(1, "read", r#"{"path":"a","bogus":"x"}"#);
        s.call(1, "read", r#"{"path":"a"}"#);
        assert_eq!(
            classes(&s),
            vec![
                Class::UnknownTool,
                Class::InvalidJson,
                Class::InvalidStructure,
                Class::InvalidValue,
                Class::InvalidStructure,
                Class::Precondition,
            ]
        );
    }

    #[test]
    fn repeat_and_repair_follow_the_record_rules() {
        let mut s = sandbox();
        s.call(1, "read", r#"{"path":"a"}"#);
        s.call(2, "read", r#"{"path":"a"}"#);
        s.call(3, "write", r#"{"path":"a","content":"x"}"#);
        s.call(4, "read", r#"{"path":"a"}"#);
        assert_eq!(
            classes(&s),
            vec![
                Class::Precondition,
                Class::RepeatIdentical,
                Class::Ok,
                Class::OkAfterRepair
            ]
        );
    }

    #[test]
    fn invalid_classes_are_the_numerator_set() {
        assert!(is_invalid(Class::UnknownTool) && is_invalid(Class::InvalidValue));
        assert!(!is_invalid(Class::Precondition) && !is_invalid(Class::ToolError));
        assert_eq!(class_name(Class::OkAfterRepair), "ok_after_repair");
    }

    #[test]
    fn expected_call_matcher_uses_subset_wildcard_and_distinct_calls() {
        let want = |tool: &str, args: Value| ExpectedCall {
            tool: tool.into(),
            args: args.as_object().cloned().unwrap_or_default(),
        };
        let got = |tool: &str, args: Value| (tool.to_owned(), args.as_object().cloned().unwrap());
        let calls = vec![got("write", json!({"path":"a","content":"x"}))];
        assert!(expected_calls_met(&[want("write", json!({"path":"a"}))], &calls).is_ok());
        assert!(expected_calls_met(&[want("write", json!({"content":"*"}))], &calls).is_ok());
        assert!(expected_calls_met(&[want("write", json!({"path":"b"}))], &calls).is_err());
        assert!(expected_calls_met(&[want("write", json!({"content":"contains:x"}))], &calls).is_ok());
        assert!(expected_calls_met(&[want("write", json!({"content":"contains:y"}))], &calls).is_err());
        assert!(expected_calls_met(&[want("read", json!({}))], &calls).is_err());
        let twice = [want("write", json!({})), want("write", json!({}))];
        assert!(expected_calls_met(&twice, &calls).is_err());
    }

    #[test]
    fn state_predicates_and_answer_checks() {
        let entries = BTreeMap::from([("a".to_owned(), "hello world".to_owned())]);
        let p = |s: &str| serde_json::from_str::<Predicate>(s).unwrap();
        assert!(predicates_hold(&[p(r#"{"entry_exists":{"key":"a"}}"#)], &entries).is_ok());
        assert!(predicates_hold(&[p(r#"{"entry_absent":{"key":"b"}}"#)], &entries).is_ok());
        assert!(
            predicates_hold(
                &[p(r#"{"entry_contains":{"key":"a","text":"world"}}"#)],
                &entries
            )
            .is_ok()
        );
        assert!(
            predicates_hold(
                &[p(r#"{"entry_equals":{"key":"a","value":"hello"}}"#)],
                &entries
            )
            .is_err()
        );
        assert!(answer_holds(&["ŠTYRI".into()], "Je ich štyri.").is_ok());
        assert!(answer_holds(&["five".into()], "four").is_err());
    }
}
