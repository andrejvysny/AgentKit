//! Presentation layer (native tool use T3.1, ADR 0031).
//!
//! A [`Dialect`] is versioned data plus two pure functions. `render` turns the
//! canonical tool definitions into what the model is offered (wire names,
//! parameter shapes); `translate` turns a call the model emitted back into
//! the canonical call. Canonical ids, schemas, guards, receipts and stored
//! data never change: guards see the call [`Dialect::translate`] returns.
//!
//! Only the coding tools differ between dialects; every other tool passes
//! through with the same name and schema. Names that other tools' text
//! mentions are rewritten to the names this dialect offers.

mod common;
mod history;
mod text;

pub use common::{COMMON_ID, COMMON_VERSION, common};
pub use history::present_history;

use crate::ToolDefinition;
use agentkit_contracts::{AiModelToolProfile, AiSessionPrefix, Optional};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// How one canonical tool is offered: one wire tool, or several when the
/// canonical tool is split by a parameter (`git_inspect` by `command`).
#[derive(Clone, Debug, PartialEq)]
pub struct ToolForm {
    pub canonical: String,
    pub wires: Vec<WireTool>,
}

/// One tool the model sees.
#[derive(Clone, Debug, PartialEq)]
pub struct WireTool {
    pub name: String,
    /// Canonical parameters this wire tool offers; `None` offers all of them.
    pub params: Option<Vec<String>>,
    /// Canonical arguments this wire tool always means. They are not offered
    /// as parameters and translation adds them to the canonical call.
    pub fixed: Vec<(String, Value)>,
    /// Replaces the canonical description (needed when the canonical text
    /// describes the split tool as a whole).
    pub description: Option<String>,
}

/// A call translated to canonical form.
#[derive(Clone, Debug, PartialEq)]
pub struct Translated {
    pub name: String,
    pub args: Value,
    /// True when `args` differs from what the model sent.
    pub args_changed: bool,
}

/// A call the dialect cannot translate; the model reads the message.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct DialectError {
    pub message: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Dialect {
    pub id: String,
    pub version: String,
    pub tools: Vec<ToolForm>,
}

impl Dialect {
    /// The same forms under another id (a dialect that has no data of its
    /// own yet).
    pub fn with_id(mut self, id: &str) -> Self {
        self.id = id.to_owned();
        self
    }

    fn form(&self, canonical: &str) -> Option<&ToolForm> {
        self.tools.iter().find(|form| form.canonical == canonical)
    }

    fn find_wire(&self, wire: &str) -> Option<(&ToolForm, &WireTool)> {
        self.tools.iter().find_map(|form| {
            form.wires
                .iter()
                .find(|tool| tool.name == wire)
                .map(|tool| (form, tool))
        })
    }

    /// True for a name this dialect offers for a coding tool.
    pub fn knows_wire(&self, wire: &str) -> bool {
        self.find_wire(wire).is_some()
    }

    /// The canonical id behind a wire name this dialect offers.
    pub fn canonical_of(&self, wire: &str) -> Option<&str> {
        self.find_wire(wire).map(|(form, _)| form.canonical.as_str())
    }

    /// Every wire name this dialect offers for `canonical` (empty for a tool
    /// that passes through).
    pub fn wire_names_of(&self, canonical: &str) -> Vec<String> {
        self.form(canonical)
            .map(|form| form.wires.iter().map(|tool| tool.name.clone()).collect())
            .unwrap_or_default()
    }

    /// Every coding wire name, in table order.
    pub fn wire_names(&self) -> Vec<String> {
        self.tools
            .iter()
            .flat_map(|form| form.wires.iter().map(|tool| tool.name.clone()))
            .collect()
    }

    /// Canonical ids mapped to the text that replaces them in descriptions.
    fn name_rewrites(&self) -> BTreeMap<String, String> {
        let mut names = BTreeMap::new();
        for form in &self.tools {
            let wires: Vec<&str> = form.wires.iter().map(|tool| tool.name.as_str()).collect();
            let shown = match wires.as_slice() {
                [] => continue,
                [only] => (*only).to_owned(),
                [rest @ .., last] => format!("{} or {last}", rest.join(", ")),
            };
            if shown != form.canonical {
                names.insert(form.canonical.clone(), shown);
            }
        }
        names
    }

    /// The definitions the model is offered for `canonical`. Pure: the same
    /// input gives the same output.
    pub fn render(&self, canonical: &[ToolDefinition]) -> Vec<ToolDefinition> {
        let names = self.name_rewrites();
        let mut out = Vec::with_capacity(canonical.len());
        for definition in canonical {
            match self.form(&definition.name) {
                Some(form) => {
                    for wire in &form.wires {
                        out.push(wire_definition(definition, wire, &names));
                    }
                }
                None => out.push(passthrough(definition, &names)),
            }
        }
        out
    }

    /// Wire names that more than one offered tool would carry (a tool from
    /// elsewhere named like a coding wire name). The caller refuses or
    /// reports them: two tools never share a name in one session.
    pub fn collisions(&self, canonical: &[ToolDefinition]) -> Vec<String> {
        let mut seen = BTreeSet::new();
        let mut repeated = BTreeSet::new();
        for definition in self.render(canonical) {
            if !seen.insert(definition.name.clone()) {
                repeated.insert(definition.name);
            }
        }
        repeated.into_iter().collect()
    }

    /// The canonical call for a call the model emitted. A name this dialect
    /// does not offer passes through unchanged. Pure.
    pub fn translate(&self, name: &str, args: Value) -> Result<Translated, DialectError> {
        let Some((form, wire)) = self.find_wire(name) else {
            return Ok(Translated {
                name: name.to_owned(),
                args,
                args_changed: false,
            });
        };
        let mut translated = Translated {
            name: form.canonical.clone(),
            args,
            args_changed: false,
        };
        if wire.fixed.is_empty() {
            return Ok(translated);
        }
        let Some(object) = translated.args.as_object_mut() else {
            return Err(DialectError {
                message: format!(
                    "Tool {name} takes a JSON object of arguments. Send the arguments as one JSON object."
                ),
            });
        };
        for (key, value) in &wire.fixed {
            match object.get(key) {
                Some(given) if given != value => {
                    return Err(DialectError {
                        message: format!(
                            "Tool {name} always means {key} {value}; do not send {key}. Use the tool that matches the {key} you want."
                        ),
                    });
                }
                Some(_) => {}
                None => {
                    object.insert(key.clone(), value.clone());
                    translated.args_changed = true;
                }
            }
        }
        Ok(translated)
    }

    /// The inverse of [`Dialect::translate`]: the wire tool and arguments a
    /// canonical call is shown as (history sent back to the model). `None`
    /// when no wire tool of this dialect can express the call.
    pub fn wire_call(&self, canonical: &str, args: &Value) -> Option<(String, Value)> {
        let Some(form) = self.form(canonical) else {
            return Some((canonical.to_owned(), args.clone()));
        };
        for wire in &form.wires {
            if wire.fixed.is_empty() {
                return Some((wire.name.clone(), args.clone()));
            }
            let object = args.as_object()?;
            if wire.fixed.iter().all(|(key, value)| object.get(key) == Some(value)) {
                let mut object = object.clone();
                for (key, _) in &wire.fixed {
                    object.remove(key);
                }
                return Some((wire.name.clone(), Value::Object(object)));
            }
        }
        None
    }
}

fn passthrough(definition: &ToolDefinition, names: &BTreeMap<String, String>) -> ToolDefinition {
    let mut out = definition.clone();
    out.description = text::rewrite(&out.description, names);
    text::rewrite_schema(&mut out.input_schema, names);
    out
}

fn wire_definition(
    definition: &ToolDefinition,
    wire: &WireTool,
    names: &BTreeMap<String, String>,
) -> ToolDefinition {
    let mut out = definition.clone();
    out.name = wire.name.clone();
    if let Some(description) = &wire.description {
        out.description = description.clone();
    } else {
        out.description = text::rewrite(&out.description, names);
    }
    if wire.params.is_some() || !wire.fixed.is_empty() {
        let kept = shape_schema(&mut out.input_schema, wire);
        out.examples = wire_examples(&definition.examples, wire, &kept);
    }
    text::rewrite_schema(&mut out.input_schema, names);
    out
}

/// Restricts the schema's properties to what `wire` offers and returns their
/// names. Required lists lose the names that are gone.
fn shape_schema(schema: &mut Value, wire: &WireTool) -> BTreeSet<String> {
    let mut kept = BTreeSet::new();
    if let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut) {
        properties.retain(|key, _| {
            !wire.fixed.iter().any(|(fixed, _)| fixed == key)
                && wire
                    .params
                    .as_ref()
                    .is_none_or(|params| params.iter().any(|param| param == key))
        });
        kept.extend(properties.keys().cloned());
    }
    if let Some(required) = schema.get_mut("required").and_then(Value::as_array_mut) {
        required.retain(|name| name.as_str().is_some_and(|name| kept.contains(name)));
    }
    kept
}

/// The canonical examples that this wire tool can express, as wire examples.
fn wire_examples(examples: &[Value], wire: &WireTool, kept: &BTreeSet<String>) -> Vec<Value> {
    examples
        .iter()
        .filter_map(|example| {
            let mut object = example.as_object()?.clone();
            for (key, value) in &wire.fixed {
                if object.remove(key).as_ref() != Some(value) {
                    return None;
                }
            }
            object
                .keys()
                .all(|key| kept.contains(key))
                .then_some(Value::Object(object))
        })
        .collect()
}

/// The dialect with this id and version, if one is known.
///
/// TODO(T3.2, T3.3): `anthropic` and `openai` have no forms of their own yet
/// (experiment X3 decides whether vendor tools beat the common shape); they
/// map to the common forms under their own id until then.
pub fn lookup(id: &str, version: &str) -> Option<Dialect> {
    if version != COMMON_VERSION {
        return None;
    }
    match id {
        COMMON_ID => Some(common()),
        "anthropic" | "openai" => Some(common().with_id(id)),
        _ => None,
    }
}

/// The dialect a session is offered tools in, or `None` for a session whose
/// frozen tool list predates dialects (its manifest holds canonical names and
/// stays as it is, so old sessions replay).
///
/// A new session uses the profile's dialect, `common` without a profile. A
/// profile naming an unknown dialect or version falls back to `common`.
pub fn session_dialect(
    prior: Option<&AiSessionPrefix>,
    profile: Option<&AiModelToolProfile>,
) -> Option<Dialect> {
    let legacy = prior.is_some_and(|prefix| {
        matches!(prefix.tool_manifest, Optional::Present(_))
            && matches!(prefix.dialect_version, Optional::Absent)
    });
    if legacy {
        return None;
    }
    let chosen = profile.and_then(|profile| lookup(&profile.dialect.id, &profile.dialect.version));
    Some(chosen.unwrap_or_else(common))
}
