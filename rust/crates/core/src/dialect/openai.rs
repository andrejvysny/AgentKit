//! The OpenAI dialect, version 1 (T3.3, TU-26).
//!
//! GPT models are trained on two client tools of the Responses API: the patch
//! tool (`{"type":"apply_patch"}`) and the shell tool
//! (`{"type":"shell","environment":{"type":"local"}}`). Every other tool keeps
//! the common shape with a strict schema (see [`super::strict`]).
//!
//! Vendor formats, read 2026-11-15 from developers.openai.com (see
//! docs/evidence/tool-use/T3.3):
//! - `apply_patch_call` carries `call_id` and one `operation`
//!   `{type: create_file | update_file | delete_file, path, diff}` (no `diff`
//!   for delete). The answer is `apply_patch_call_output`
//!   `{call_id, status: completed | failed, output}`. The diff is V4A: `@@`
//!   starts a hunk, `-` removes, `+` adds, a space or nothing is context.
//! - `shell_call` carries `call_id` and `action`
//!   `{commands: [string], timeout_ms, max_output_length}`. The answer is
//!   `shell_call_output` with one `{stdout, stderr, outcome}` per command,
//!   `outcome` being `{type:"exit", exit_code}` or `{type:"timeout"}`.
//!
//! A built-in tool is carried through the presentation layer as a
//! [`ToolDefinition`] whose `input_schema` holds the marker [`BUILTIN_KEY`]
//! with the vendor tool type; the Responses request builder serializes it as
//! the typed tool, and the call arrives with the vendor's item as arguments
//! (the `operation`, the `action`).
//!
//! One canonical call per call. A patch operation names one file, so the
//! multi-file question never arises; the diff goes to `edit_file` as its
//! `patch` argument and is applied there with V4A hunk semantics (whole-line
//! matching, anchors, in-order hunks), not as a substring replacement. A
//! `delete_file` is refused: no canonical tool deletes. A shell call with
//! several commands is refused for the same reason.
//!
//! On Chat Completions the dialect is [`OPENAI_CHAT_ID`]: the common shape,
//! strict schemas, no built-in tools.

use super::strict::{fill_nulls, strict_schema};
use super::{BUILTIN_KEY, Dialect, DialectError, Translated, WireTool, common};
use crate::ToolDefinition;
use serde_json::{Map, Value, json};

pub const OPENAI_ID: &str = "openai";
pub const OPENAI_CHAT_ID: &str = "openai-chat";
pub const PATCH_NAME: &str = "apply_patch";
pub const SHELL_NAME: &str = "shell";

const PATCH_TOOLS: [&str; 2] = ["write_file", "edit_file"];
const RUN_COMMAND: &str = "run_command";

impl Dialect {
    /// True for a wire name that is a vendor built-in tool of this dialect
    /// (T3.2, T3.3): one wire tool reaching several canonical tools or
    /// carrying the vendor's own argument shape.
    pub fn built_in(&self, wire: &str) -> bool {
        match self.id.as_str() {
            super::ANTHROPIC_ID => wire == super::TEXT_EDITOR_NAME || wire == super::BASH_NAME,
            OPENAI_ID => wire == PATCH_NAME || wire == SHELL_NAME,
            _ => false,
        }
    }
}

/// True for the id of a dialect whose schemas are strict (OpenAI, both lanes).
pub fn strict_dialect(id: &str) -> bool {
    id == OPENAI_ID || id == OPENAI_CHAT_ID
}

impl Dialect {
    /// True for a dialect whose schemas are strict.
    pub fn strict(&self) -> bool {
        strict_dialect(&self.id)
    }
}

fn wire(name: &str) -> WireTool {
    WireTool {
        name: name.to_owned(),
        params: None,
        fixed: vec![],
        description: None,
    }
}

/// OpenAI v1 on the Responses lane: the common forms with the coding tools
/// folded into the patch and shell tools.
pub fn openai() -> Dialect {
    let mut dialect = common().with_id(OPENAI_ID);
    for form in &mut dialect.tools {
        if PATCH_TOOLS.contains(&form.canonical.as_str()) {
            form.wires = vec![wire(PATCH_NAME)];
        } else if form.canonical == RUN_COMMAND {
            form.wires = vec![wire(SHELL_NAME)];
        }
    }
    dialect
}

/// OpenAI v1 on the Chat Completions lane: the common shape, strict schemas.
pub fn openai_chat() -> Dialect {
    common().with_id(OPENAI_CHAT_ID)
}

/// The form table with the built-in forms replaced by the common ones for the
/// tools that are not offered as built-ins.
fn effective(dialect: &Dialect, patch: bool, shell: bool) -> Dialect {
    let plain = common();
    let mut out = dialect.clone();
    for form in &mut out.tools {
        let swap = (PATCH_TOOLS.contains(&form.canonical.as_str()) && !patch)
            || (form.canonical == RUN_COMMAND && !shell);
        if swap && let Some(original) = plain.tools.iter().find(|f| f.canonical == form.canonical) {
            *form = original.clone();
        }
    }
    out
}

fn builtin(name: &str, base: &ToolDefinition, effect: &str) -> ToolDefinition {
    ToolDefinition {
        name: name.to_owned(),
        version: base.version.clone(),
        effect: effect.to_owned(),
        capability: base.capability.clone(),
        // The model knows the tool; the vendor takes no description.
        description: String::new(),
        input_schema: json!({ "type": "object", BUILTIN_KEY: name }),
        output_schema: None,
        timeout_ms: base.timeout_ms,
        examples: vec![],
    }
}

/// Strict rewrite of every ordinary definition; built-ins and tools whose
/// schema cannot be strict stay as they are.
fn make_strict(definitions: &mut [ToolDefinition]) {
    for definition in definitions {
        if definition.input_schema.get(BUILTIN_KEY).is_some() {
            continue;
        }
        if let Some(strict) = strict_schema(&definition.input_schema) {
            definition.examples = definition
                .examples
                .iter()
                .map(|example| fill_nulls(example, &definition.input_schema))
                .collect();
            definition.input_schema = strict;
        }
    }
}

/// The Chat Completions variant renders the common forms, then strict.
pub(super) fn render_chat(dialect: &Dialect, canonical: &[ToolDefinition]) -> Vec<ToolDefinition> {
    let mut out = dialect.render_forms(canonical);
    make_strict(&mut out);
    out
}

/// The patch tool stands for the canonical write and edit tools and is offered
/// when either is present; the shell tool when `run_command` is.
pub(super) fn render(dialect: &Dialect, canonical: &[ToolDefinition]) -> Vec<ToolDefinition> {
    let has = |name: &str| canonical.iter().any(|definition| definition.name == name);
    let patch = PATCH_TOOLS.iter().any(|name| has(name));
    let shell = has(RUN_COMMAND);
    let effective = effective(dialect, patch, shell);
    let mut out = Vec::with_capacity(canonical.len());
    let mut patch_done = false;
    for definition in canonical {
        let name = definition.name.as_str();
        if patch && PATCH_TOOLS.contains(&name) {
            if !patch_done {
                patch_done = true;
                // The patch tool can write, so it carries the write effect.
                out.push(builtin(PATCH_NAME, definition, "write"));
            }
        } else if shell && name == RUN_COMMAND {
            out.push(builtin(SHELL_NAME, definition, &definition.effect));
        } else {
            out.extend(effective.render_forms(std::slice::from_ref(definition)));
        }
    }
    make_strict(&mut out);
    out
}

fn fail(message: &str) -> DialectError {
    DialectError {
        message: message.to_owned(),
    }
}

fn translated(name: &str, args: Value) -> Translated {
    Translated {
        name: name.to_owned(),
        args,
        args_changed: true,
    }
}

/// A call to a built-in tool as the canonical call, or `None` when `name` is
/// not a built-in tool of this dialect.
pub(super) fn translate(name: &str, args: &Value) -> Option<Result<Translated, DialectError>> {
    match name {
        PATCH_NAME => Some(translate_patch(args)),
        SHELL_NAME => Some(translate_shell(args)),
        _ => None,
    }
}

fn object<'a>(tool: &str, args: &'a Value) -> Result<&'a Map<String, Value>, DialectError> {
    args.as_object().ok_or_else(|| {
        fail(&format!(
            "Tool {tool} takes a JSON object of arguments. Send the arguments as one JSON object."
        ))
    })
}

fn only(tool: &str, object: &Map<String, Value>, allowed: &[&str]) -> Result<(), DialectError> {
    match object.keys().find(|key| !allowed.contains(&key.as_str())) {
        Some(key) => Err(fail(&format!(
            "Tool {tool} does not take {key}. It takes: {}.",
            allowed.join(", ")
        ))),
        None => Ok(()),
    }
}

fn string(tool: &str, object: &Map<String, Value>, key: &str) -> Result<String, DialectError> {
    match object.get(key) {
        Some(Value::String(value)) => Ok(value.clone()),
        _ => Err(fail(&format!(
            "Tool {tool} needs {key} as a string. Send {key}."
        ))),
    }
}

fn translate_patch(args: &Value) -> Result<Translated, DialectError> {
    let tool = PATCH_NAME;
    let object = object(tool, args)?;
    only(tool, object, &["type", "path", "diff"])?;
    let kind = string(tool, object, "type")?;
    let path = string(tool, object, "path")?;
    if path.is_empty() {
        return Err(fail("Tool apply_patch needs a non-empty path."));
    }
    match kind.as_str() {
        "create_file" => {
            let diff = string(tool, object, "diff")?;
            let content = created_content(&diff)?;
            Ok(translated(
                "write_file",
                json!({ "file_path": path, "content": content }),
            ))
        }
        "update_file" => {
            let diff = string(tool, object, "diff")?;
            // The hunks go to the edit tool whole: it matches whole lines at
            // the hunk positions against the snapshot it read.
            if diff.trim().is_empty() {
                return Err(fail(
                    "Tool apply_patch update_file needs a diff with at least one hunk: @@ then context ( ), removed (-) and added (+) lines.",
                ));
            }
            Ok(translated(
                "edit_file",
                json!({ "file_path": path, "patch": diff }),
            ))
        }
        "delete_file" => Err(fail(
            "Tool apply_patch delete_file is not available here: nothing in this session deletes files. Do not retry it; tell the user the file needs removing, or remove it with the shell tool if they asked for that.",
        )),
        other => Err(fail(&format!(
            "Tool apply_patch does not know operation {other}. Use create_file or update_file."
        ))),
    }
}

/// The lines of a diff: one trailing newline ends the last line, it does not
/// start another.
fn diff_lines(diff: &str) -> (Vec<&str>, bool) {
    if diff.is_empty() {
        return (vec![], false);
    }
    let (body, terminated) = match diff.strip_suffix('\n') {
        Some(body) => (body, true),
        None => (diff, false),
    };
    (body.split('\n').collect(), terminated)
}

/// A create diff is the whole file, each line behind `+`.
fn created_content(diff: &str) -> Result<String, DialectError> {
    let (lines, terminated) = diff_lines(diff);
    let mut content = Vec::with_capacity(lines.len());
    for line in lines {
        match line.strip_prefix('+') {
            Some(text) => content.push(text),
            None => {
                return Err(fail(
                    "Tool apply_patch create_file diff holds the whole new file with every line behind +. Put + at the start of each line.",
                ));
            }
        }
    }
    let mut out = content.join("\n");
    if terminated {
        out.push('\n');
    }
    Ok(out)
}

fn translate_shell(args: &Value) -> Result<Translated, DialectError> {
    let tool = SHELL_NAME;
    let object = object(tool, args)?;
    only(
        tool,
        object,
        &["commands", "timeout_ms", "max_output_length"],
    )?;
    let commands = object
        .get("commands")
        .and_then(Value::as_array)
        .ok_or_else(|| fail("Tool shell needs commands as a list of command strings."))?;
    let [command] = commands.as_slice() else {
        return Err(fail(
            "Tool shell runs one command per call. Send one command in commands; to chain steps write them as one command, for example cd dir && make.",
        ));
    };
    let Some(command) = command.as_str().filter(|command| !command.is_empty()) else {
        return Err(fail(
            "Tool shell needs a non-empty command string in commands.",
        ));
    };
    let mut args = json!({ "command": command });
    match object.get("timeout_ms") {
        None | Some(Value::Null) => {}
        Some(timeout) => args["timeout"] = timeout.clone(),
    }
    Ok(translated("run_command", args))
}

/// The built-in call that shows canonical call `canonical` (history sent back
/// to the model). The outer `None`: this dialect has no built-in for the
/// tool. The inner `None`: it has one but cannot express these arguments.
pub(super) fn wire_call(canonical: &str, args: &Value) -> Option<Option<(String, Value)>> {
    if !PATCH_TOOLS.contains(&canonical) && canonical != RUN_COMMAND {
        return None;
    }
    Some(wire_args(canonical, args))
}

fn wire_args(canonical: &str, args: &Value) -> Option<(String, Value)> {
    let object = args.as_object()?;
    let allowed = |keys: &[&str]| object.keys().all(|key| keys.contains(&key.as_str()));
    let text = |key: &str| object.get(key).and_then(Value::as_str);
    match canonical {
        "write_file" if allowed(&["file_path", "content"]) => {
            let (lines, terminated) = diff_lines(text("content")?);
            let mut diff = lines
                .iter()
                .map(|line| format!("+{line}"))
                .collect::<Vec<_>>()
                .join("\n");
            if terminated {
                diff.push('\n');
            }
            Some((
                PATCH_NAME.to_owned(),
                json!({ "type": "create_file", "path": text("file_path")?, "diff": diff }),
            ))
        }
        "edit_file" if allowed(&["file_path", "patch"]) => Some((
            PATCH_NAME.to_owned(),
            json!({ "type": "update_file", "path": text("file_path")?, "diff": text("patch")? }),
        )),
        // Exact only: replace_all has no patch form, and an empty
        // replacement would come back with an added line end.
        "edit_file" if allowed(&["file_path", "old_string", "new_string"]) => {
            let (old, new) = (text("old_string")?, text("new_string")?);
            if old.is_empty() || new.is_empty() {
                return None;
            }
            let mut diff = String::from("@@\n");
            for line in old.split('\n') {
                diff.push_str(&format!("-{line}\n"));
            }
            for line in new.split('\n') {
                diff.push_str(&format!("+{line}\n"));
            }
            Some((
                PATCH_NAME.to_owned(),
                json!({ "type": "update_file", "path": text("file_path")?, "diff": diff }),
            ))
        }
        "run_command" if allowed(&["command", "timeout"]) => {
            let mut action = json!({ "commands": [text("command")?] });
            if let Some(timeout) = object.get("timeout") {
                action["timeout_ms"] = timeout.clone();
            }
            Some((SHELL_NAME.to_owned(), action))
        }
        _ => None,
    }
}
