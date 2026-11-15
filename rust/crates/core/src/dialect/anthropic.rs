//! The Anthropic dialect, version 1 (T3.2, TU-25).
//!
//! Anthropic's models are trained on two client tools whose schemas are built
//! into the model: the text editor (`str_replace_based_edit_tool`) and bash
//! (`bash`). They are offered as typed tools with no description and no
//! `input_schema`; the model calls them with the vendor's argument shapes and
//! this module translates those calls to the canonical ones. Search, git, plan
//! and every other tool keep the common shape.
//!
//! Vendor formats (read from the vendor docs, see docs/evidence/tool-use/T3.2):
//! text editor `text_editor_20250728`, commands `view`, `str_replace`,
//! `create`, `insert` (`undo_edit` was removed in `text_editor_20250429`);
//! bash `bash_20250124`, input `command` or `restart`.
//!
//! A built-in tool is carried through the presentation layer as a
//! [`ToolDefinition`] whose `input_schema` holds the marker [`BUILTIN_KEY`]
//! with the vendor tool type. [`builtin_tool_type`] reads it; a request
//! builder serializes such a definition as `{type, name}`.
//!
//! The text editor reaches the canonical read, write and edit tools. It is
//! offered only when all three are present, so the model never holds a wider
//! tool than the session has; otherwise those tools keep their common form.
//! Nothing is invented: a call the canonical tools cannot express (`insert`,
//! `undo_edit`, bash `restart`) is refused with text that tells the model
//! what to do instead.

use super::{common, Dialect, DialectError, ToolForm, Translated, WireTool};
use crate::ToolDefinition;
use serde_json::{json, Map, Value};

pub const ANTHROPIC_ID: &str = "anthropic";
pub const TEXT_EDITOR_TYPE: &str = "text_editor_20250728";
pub const TEXT_EDITOR_NAME: &str = "str_replace_based_edit_tool";
pub const BASH_TYPE: &str = "bash_20250124";
pub const BASH_NAME: &str = "bash";
/// Schema key that marks a definition as a vendor built-in tool; its value is
/// the vendor tool `type`.
pub const BUILTIN_KEY: &str = "x-builtin-tool";

/// The canonical tools the text editor needs, all of them.
const EDITOR_REQUIRED: [&str; 3] = ["read_file", "write_file", "edit_file"];
/// Every canonical tool the text editor reaches (a directory `view` lists).
const EDITOR_TOOLS: [&str; 4] = ["read_file", "write_file", "edit_file", "list_dir"];
const RUN_COMMAND: &str = "run_command";

/// The vendor tool type of a built-in definition, or `None` for an ordinary
/// tool (which is sent with a description and an `input_schema`).
pub fn builtin_tool_type(definition: &ToolDefinition) -> Option<&str> {
    definition.input_schema.get(BUILTIN_KEY)?.as_str()
}

fn wire(name: &str) -> WireTool {
    WireTool {
        name: name.to_owned(),
        params: None,
        fixed: vec![],
        description: None,
    }
}

/// Anthropic v1: the common forms, with the coding tools folded into the two
/// built-in tools.
pub fn anthropic() -> Dialect {
    let mut dialect = common().with_id(ANTHROPIC_ID);
    for form in &mut dialect.tools {
        if EDITOR_TOOLS.contains(&form.canonical.as_str()) {
            form.wires = vec![wire(TEXT_EDITOR_NAME)];
        } else if form.canonical == RUN_COMMAND {
            form.wires = vec![wire(BASH_NAME)];
        }
    }
    dialect
}

impl Dialect {
    /// The dialect as it renders `canonical`: for Anthropic, the built-in
    /// forms are kept only where their prerequisites are offered and the
    /// common forms stand in elsewhere. A session freezes this table, so the
    /// call a model sends for what it was shown (`list_directory`, `edit`)
    /// translates and shows back exactly as rendered. Other dialects are
    /// returned as they are.
    pub fn effective_for(&self, canonical: &[ToolDefinition]) -> Dialect {
        if self.id != ANTHROPIC_ID {
            return self.clone();
        }
        let has = |name: &str| canonical.iter().any(|definition| definition.name == name);
        self.effective(EDITOR_REQUIRED.iter().all(|name| has(name)), has(RUN_COMMAND))
    }

    /// This dialect's table with the built-in forms replaced by the common
    /// ones for the tools that are not offered as built-ins.
    fn effective(&self, editor: bool, bash: bool) -> Dialect {
        let plain = common();
        let mut out = self.clone();
        for form in &mut out.tools {
            let editor_form = EDITOR_TOOLS.contains(&form.canonical.as_str());
            let bash_form = form.canonical == RUN_COMMAND;
            let swap = (editor_form && !editor) || (bash_form && !bash);
            if swap
                && let Some(original) = plain.tools.iter().find(|f| f.canonical == form.canonical)
            {
                *form = original.clone();
            }
        }
        out
    }
}

fn builtin(kind: &str, name: &str, base: &ToolDefinition, effect: &str) -> ToolDefinition {
    ToolDefinition {
        name: name.to_owned(),
        version: base.version.clone(),
        effect: effect.to_owned(),
        capability: base.capability.clone(),
        // A built-in tool has no description: its documentation is in the
        // model. Examples are skipped for the same reason.
        description: String::new(),
        input_schema: json!({ "type": "object", BUILTIN_KEY: kind }),
        output_schema: None,
        timeout_ms: base.timeout_ms,
        examples: vec![],
    }
}

pub(super) fn render(dialect: &Dialect, canonical: &[ToolDefinition]) -> Vec<ToolDefinition> {
    let has = |name: &str| canonical.iter().any(|definition| definition.name == name);
    let editor = EDITOR_REQUIRED.iter().all(|name| has(name));
    let bash = has(RUN_COMMAND);
    let effective = dialect.effective(editor, bash);
    let mut out = Vec::with_capacity(canonical.len());
    let mut editor_done = false;
    for definition in canonical {
        let name = definition.name.as_str();
        if editor && EDITOR_TOOLS.contains(&name) {
            if !editor_done {
                editor_done = true;
                let base = canonical
                    .iter()
                    .find(|d| d.name == "edit_file")
                    .unwrap_or(definition);
                // The editor can write, so it carries the write effect.
                out.push(builtin(TEXT_EDITOR_TYPE, TEXT_EDITOR_NAME, base, "write"));
            }
        } else if bash && name == RUN_COMMAND {
            out.push(builtin(
                BASH_TYPE,
                BASH_NAME,
                definition,
                &definition.effect,
            ));
        } else {
            out.extend(effective.render_forms(std::slice::from_ref(definition)));
        }
    }
    out
}

fn fail(message: String) -> DialectError {
    DialectError { message }
}

fn object<'a>(tool: &str, args: &'a Value) -> Result<&'a Map<String, Value>, DialectError> {
    args.as_object().ok_or_else(|| {
        fail(format!(
            "Tool {tool} takes a JSON object of arguments. Send the arguments as one JSON object."
        ))
    })
}

fn only(
    tool: &str,
    what: &str,
    object: &Map<String, Value>,
    allowed: &[&str],
) -> Result<(), DialectError> {
    match object.keys().find(|key| !allowed.contains(&key.as_str())) {
        Some(key) => Err(fail(format!(
            "Tool {tool} {what} does not take {key}. It takes: {}.",
            allowed.join(", ")
        ))),
        None => Ok(()),
    }
}

fn text(
    tool: &str,
    command: &str,
    object: &Map<String, Value>,
    key: &str,
) -> Result<String, DialectError> {
    match object.get(key) {
        Some(Value::String(value)) => Ok(value.clone()),
        _ => Err(fail(format!(
            "Tool {tool} {command} needs {key} as a string. Send {key}."
        ))),
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
        TEXT_EDITOR_NAME => Some(translate_editor(args)),
        BASH_NAME => Some(translate_bash(args)),
        _ => None,
    }
}

fn translate_editor(args: &Value) -> Result<Translated, DialectError> {
    let tool = TEXT_EDITOR_NAME;
    let object = object(tool, args)?;
    let Some(command) = object.get("command").and_then(Value::as_str) else {
        return Err(fail(format!(
            "Tool {tool} needs command: one of view, str_replace, create. Send command."
        )));
    };
    match command {
        "view" => {
            only(tool, "view", object, &["command", "path", "view_range"])?;
            translate_view(object)
        }
        "str_replace" => {
            only(tool, "str_replace", object, &["command", "path", "old_str", "new_str"])?;
            Ok(translated(
                "edit_file",
                json!({
                    "file_path": text(tool, command, object, "path")?,
                    "old_string": text(tool, command, object, "old_str")?,
                    "new_string": text(tool, command, object, "new_str")?,
                    "replace_all": false,
                }),
            ))
        }
        "create" => {
            only(tool, "create", object, &["command", "path", "file_text"])?;
            Ok(translated(
                "write_file",
                json!({
                    "file_path": text(tool, command, object, "path")?,
                    "content": text(tool, command, object, "file_text")?,
                }),
            ))
        }
        "insert" => Err(fail(format!(
            "Tool {tool} insert is not available here. Use str_replace: view the file, then send old_str with the line next to the insertion point and new_str with that line plus the text to add. For a new file use create."
        ))),
        "undo_edit" => Err(fail(format!(
            "Tool {tool} undo_edit is not available. Undo a change with str_replace: send the new text as old_str and the previous text as new_str."
        ))),
        other => Err(fail(format!(
            "Tool {tool} does not know command {other}. Use view, str_replace or create."
        ))),
    }
}

fn translate_view(object: &Map<String, Value>) -> Result<Translated, DialectError> {
    let tool = TEXT_EDITOR_NAME;
    let path = text(tool, "view", object, "path")?;
    if path.is_empty() {
        return Err(fail(format!("Tool {tool} view needs a non-empty path.")));
    }
    // The vendor's directory example is `src/`; `.` is the workspace root.
    if path == "." || path.ends_with('/') {
        let shown = path.trim_end_matches('/');
        let shown = if shown.is_empty() {
            path.as_str()
        } else {
            shown
        };
        return Ok(translated("list_dir", json!({ "path": shown })));
    }
    let mut args = Map::new();
    args.insert("file_path".into(), Value::String(path));
    if let Some(range) = object.get("view_range") {
        let bad = || {
            fail(format!(
                "Tool {tool} view_range is [start, end]: 1-based line numbers, end -1 for the end of the file. Send two integers."
            ))
        };
        let [start, end] = range.as_array().map(Vec::as_slice).ok_or_else(bad)? else {
            return Err(bad());
        };
        let (start, end) = (
            start.as_i64().ok_or_else(bad)?,
            end.as_i64().ok_or_else(bad)?,
        );
        if start < 1 || (end != -1 && end < start) {
            return Err(bad());
        }
        args.insert("offset".into(), json!(start));
        if end != -1 {
            args.insert("limit".into(), json!(end - start + 1));
        }
    }
    Ok(translated("read_file", Value::Object(args)))
}

fn translate_bash(args: &Value) -> Result<Translated, DialectError> {
    let tool = BASH_NAME;
    let object = object(tool, args)?;
    only(tool, "calls", object, &["command", "restart"])?;
    if object.get("restart") == Some(&Value::Bool(true)) {
        return Err(fail(
            "Tool bash restart is not available: every command runs in a fresh shell, so nothing persists between commands and nothing needs restarting. Send command instead; put cd and exports in the same command, for example cd dir && make."
                .to_owned(),
        ));
    }
    match object.get("command") {
        Some(Value::String(command)) if !command.is_empty() => {
            Ok(translated("run_command", json!({ "command": command })))
        }
        _ => Err(fail(
            "Tool bash needs command as a non-empty string. Send command.".to_owned(),
        )),
    }
}

/// The built-in call that shows canonical call `canonical` (history sent back
/// to the model). The outer `None`: this dialect has no built-in for the
/// tool. The inner `None`: it has one but cannot express these arguments.
pub(super) fn wire_call(canonical: &str, args: &Value) -> Option<Option<(String, Value)>> {
    if !EDITOR_TOOLS.contains(&canonical) && canonical != RUN_COMMAND {
        return None;
    }
    Some(wire_args(canonical, args))
}

fn wire_args(canonical: &str, args: &Value) -> Option<(String, Value)> {
    let object = args.as_object()?;
    let allowed = |keys: &[&str]| object.keys().all(|key| keys.contains(&key.as_str()));
    let string = |key: &str| object.get(key).and_then(Value::as_str);
    let editor = |input: Value| Some((TEXT_EDITOR_NAME.to_owned(), input));
    match canonical {
        "read_file" if allowed(&["file_path", "offset", "limit"]) => {
            let path = string("file_path")?;
            let mut input = json!({ "command": "view", "path": path });
            let offset = object.get("offset").map(Value::as_i64);
            let limit = object.get("limit").map(Value::as_i64);
            if offset.is_some() || limit.is_some() {
                let start = offset.map_or(Some(1), |value| value)?;
                let end = limit.map_or(Some(-1), |value| value.map(|n| start + n - 1))?;
                input["view_range"] = json!([start, end]);
            }
            editor(input)
        }
        "write_file" if allowed(&["file_path", "content"]) => editor(json!({
            "command": "create",
            "path": string("file_path")?,
            "file_text": string("content")?,
        })),
        "edit_file"
            if allowed(&["file_path", "old_string", "new_string", "replace_all"])
                && object
                    .get("replace_all")
                    .is_none_or(|v| v == &Value::Bool(false)) =>
        {
            editor(json!({
                "command": "str_replace",
                "path": string("file_path")?,
                "old_str": string("old_string")?,
                "new_str": string("new_string")?,
            }))
        }
        "list_dir" if allowed(&["path"]) => {
            let path = string("path").unwrap_or(".");
            let path = if path == "." || path.ends_with('/') {
                path.to_owned()
            } else {
                format!("{path}/")
            };
            editor(json!({ "command": "view", "path": path }))
        }
        "run_command" if allowed(&["command"]) => Some((
            BASH_NAME.to_owned(),
            json!({ "command": string("command")? }),
        )),
        _ => None,
    }
}
