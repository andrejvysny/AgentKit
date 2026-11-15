//! Tool names inside text: descriptions refer to other tools by canonical
//! id, and the model must read the names it was offered.

use serde_json::Value;
use std::collections::BTreeMap;

fn is_ident(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// `text` with every whole-identifier occurrence of a key of `names` replaced
/// by its value. One pass over identifier runs, so a replacement is never
/// rewritten again and `list_dir` never matches inside `list_directory`.
pub(super) fn rewrite(text: &str, names: &BTreeMap<String, String>) -> String {
    if names.is_empty() {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut run = String::new();
    for c in text.chars() {
        if is_ident(c) {
            run.push(c);
        } else {
            flush(&mut run, &mut out, names);
            out.push(c);
        }
    }
    flush(&mut run, &mut out, names);
    out
}

fn flush(run: &mut String, out: &mut String, names: &BTreeMap<String, String>) {
    match names.get(run.as_str()) {
        Some(replacement) => out.push_str(replacement),
        None => out.push_str(run),
    }
    run.clear();
}

/// Rewrites every `description` string anywhere in a JSON schema.
pub(super) fn rewrite_schema(schema: &mut Value, names: &BTreeMap<String, String>) {
    match schema {
        Value::Object(object) => {
            for (key, child) in object.iter_mut() {
                match child {
                    Value::String(text) if key == "description" => {
                        *text = rewrite(text, names);
                    }
                    _ => rewrite_schema(child, names),
                }
            }
        }
        Value::Array(items) => items
            .iter_mut()
            .for_each(|item| rewrite_schema(item, names)),
        _ => {}
    }
}
