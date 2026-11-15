//! Placeholders in failure text (T1.7). A tool or the pipeline writes the
//! canonical id of a tool, or a canonical call, as a placeholder; the
//! session's dialect fills in what the model was offered:
//!
//! - `{{tool:git_inspect}}` becomes the wire name(s) of the tool (`git_status,
//!   git_diff, git_log or git_show`), or the id itself when the session has no
//!   dialect or the dialect leaves the tool alone.
//! - `{{call:git_inspect command=diff}}` becomes the wire tool that always
//!   means that call (`git_diff`), or `git_inspect with command "diff"` when
//!   no wire tool does, so a split tool is never described by a parameter the
//!   model cannot send.
//!
//! An unknown kind or an unfinished placeholder is left as written.

use super::{Dialect, ToolForm};
use serde_json::Value;

/// `text` with its placeholders filled for `dialect` (canonical names when
/// there is none).
pub fn render_text(dialect: Option<&Dialect>, text: &str) -> String {
    if !text.contains("{{") {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            out.push_str(&rest[start..]);
            return out;
        };
        match fill(dialect, after[..end].trim()) {
            Some(shown) => out.push_str(&shown),
            None => out.push_str(&rest[start..start + 2 + end + 2]),
        }
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    out
}

fn fill(dialect: Option<&Dialect>, body: &str) -> Option<String> {
    if let Some(id) = body.strip_prefix("tool:") {
        return Some(shown_tool(dialect, id.trim()));
    }
    let call = body.strip_prefix("call:")?;
    let mut words = call.split_whitespace();
    let id = words.next()?;
    let fixed: Vec<(&str, &str)> = words.filter_map(|word| word.split_once('=')).collect();
    Some(shown_call(dialect, id, &fixed))
}

fn shown_tool(dialect: Option<&Dialect>, id: &str) -> String {
    dialect
        .and_then(|dialect| dialect.name_rewrites().remove(id))
        .unwrap_or_else(|| id.to_owned())
}

fn same_value(value: &Value, wanted: &str) -> bool {
    match value {
        Value::String(text) => text == wanted,
        other => other.to_string() == wanted,
    }
}

fn shown_call(dialect: Option<&Dialect>, id: &str, fixed: &[(&str, &str)]) -> String {
    let wire = dialect
        .and_then(|dialect| dialect.tools.iter().find(|form| form.canonical == id))
        .and_then(|form: &ToolForm| {
            form.wires.iter().find(|wire| {
                !fixed.is_empty()
                    && fixed.iter().all(|(key, wanted)| {
                        wire.fixed
                            .iter()
                            .any(|(name, value)| name == key && same_value(value, wanted))
                    })
            })
        });
    match wire {
        Some(wire) => wire.name.clone(),
        None => {
            let with: Vec<String> = fixed
                .iter()
                .map(|(key, value)| format!("{key} \"{value}\""))
                .collect();
            let name = shown_tool(dialect, id);
            if with.is_empty() {
                name
            } else {
                format!("{name} with {}", with.join(" and "))
            }
        }
    }
}
