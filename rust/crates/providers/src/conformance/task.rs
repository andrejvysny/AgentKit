//! Task data format. One JSON file per toolset: `{"toolset": "...", "tasks": [...]}`.
use serde::Deserialize;
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// What a fake tool does to the fake workspace (a map of key to text).
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Effect {
    /// Returns the entry named by `key_arg`; a missing entry is a precondition failure.
    Read {
        key_arg: String,
    },
    Write {
        key_arg: String,
        value_arg: String,
    },
    Append {
        key_arg: String,
        value_arg: String,
    },
    /// Appends `value_arg` and a newline to the fixed entry `key`.
    Log {
        key: String,
        value_arg: String,
    },
    /// A missing entry is a precondition failure.
    Remove {
        key_arg: String,
    },
    /// Returns fixed text and changes nothing.
    Reply {
        text: String,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ToolSpec {
    pub name: String,
    #[serde(default)]
    pub required: Vec<String>,
    #[serde(default)]
    pub optional: Vec<String>,
    pub effect: Effect,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ExpectedCall {
    pub tool: String,
    /// Subset match against the call's arguments; the string `"*"` accepts any
    /// value and `"contains:TEXT"` accepts any string containing TEXT.
    #[serde(default)]
    pub args: Map<String, Value>,
}

/// State predicate over the fake workspace after the run.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Predicate {
    EntryExists { key: String },
    EntryAbsent { key: String },
    EntryEquals { key: String, value: String },
    EntryContains { key: String, text: String },
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Expect {
    /// Each must match a distinct successful call.
    #[serde(default)]
    pub calls: Vec<ExpectedCall>,
    #[serde(default)]
    pub state: Vec<Predicate>,
    /// Substrings the final answer must contain (case-insensitive).
    #[serde(default)]
    pub answer_contains: Vec<String>,
}

/// Scripted turns for the mock model; replaces the script derived from `expect`.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct MockScript {
    pub turns: Vec<Vec<ExpectedCall>>,
    #[serde(default)]
    pub final_text: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Task {
    pub id: String,
    /// Filled from the file when absent.
    #[serde(default)]
    pub toolset: String,
    /// `en` or `sk`.
    #[serde(default = "default_lang")]
    pub lang: String,
    pub prompt: String,
    pub allowed_tools: Vec<ToolSpec>,
    #[serde(default = "default_max_turns")]
    pub max_turns: u32,
    /// Initial fake workspace.
    #[serde(default)]
    pub workspace: BTreeMap<String, String>,
    pub expect: Expect,
    #[serde(default)]
    pub mock: Option<MockScript>,
}
fn default_lang() -> String {
    "en".into()
}
fn default_max_turns() -> u32 {
    6
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskFile {
    toolset: String,
    tasks: Vec<Task>,
}

impl Task {
    fn validate(&self) -> Result<(), String> {
        let id = &self.id;
        if id.is_empty() || self.prompt.trim().is_empty() {
            return Err(format!("task '{id}': id and prompt must not be empty"));
        }
        if self.max_turns == 0 {
            return Err(format!("task '{id}': max_turns must be at least 1"));
        }
        if self.allowed_tools.is_empty() {
            return Err(format!("task '{id}': allowed_tools is empty"));
        }
        let names: BTreeSet<&str> = self.allowed_tools.iter().map(|t| t.name.as_str()).collect();
        if names.len() != self.allowed_tools.len() {
            return Err(format!("task '{id}': duplicate tool name"));
        }
        let e = &self.expect;
        if e.calls.is_empty() && e.state.is_empty() && e.answer_contains.is_empty() {
            return Err(format!("task '{id}': expect has no check"));
        }
        if let Some(call) = e.calls.iter().find(|c| !names.contains(c.tool.as_str())) {
            return Err(format!(
                "task '{id}': expected tool '{}' is not allowed",
                call.tool
            ));
        }
        Ok(())
    }
}

/// Parses one task file and validates every task. Ids must be unique in the file.
pub fn parse_task_file(text: &str) -> Result<Vec<Task>, String> {
    let file: TaskFile = serde_json::from_str(text).map_err(|e| format!("task file: {e}"))?;
    let mut seen = BTreeSet::new();
    let mut tasks = file.tasks;
    for task in &mut tasks {
        if task.toolset.is_empty() {
            task.toolset = file.toolset.clone();
        } else if task.toolset != file.toolset {
            return Err(format!(
                "task '{}': toolset '{}' differs from file toolset '{}'",
                task.id, task.toolset, file.toolset
            ));
        }
        task.validate()?;
        if !seen.insert(task.id.clone()) {
            return Err(format!("duplicate task id '{}'", task.id));
        }
    }
    Ok(tasks)
}

/// Loads every `*.json` file in `dir` in name order; ids must be unique across files.
pub fn load_dir(dir: &Path) -> Result<Vec<Task>, String> {
    let mut paths: Vec<_> = std::fs::read_dir(dir)
        .map_err(|e| format!("cannot read {}: {e}", dir.display()))?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    paths.sort();
    let mut all: Vec<Task> = Vec::new();
    for path in paths {
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let tasks = parse_task_file(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        for task in tasks {
            if all.iter().any(|t| t.id == task.id) {
                return Err(format!("duplicate task id '{}'", task.id));
            }
            all.push(task);
        }
    }
    Ok(all)
}
