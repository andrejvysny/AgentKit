//! Whether a model call is the call a block describes.

use super::Block;
use super::parse::object_of;
use super::variants::lf;
use crate::ToolCall;
use serde_json::{Map, Value};
use std::borrow::Cow;

/// One model call in the forms a block may repeat: the name as emitted and
/// as resolved, the arguments as emitted and as normalised. The call side
/// is parsed once here; it is compared with every block found.
pub(super) struct Candidate {
    names: [String; 2],
    arguments: Vec<Vec<(String, Argument)>>,
}

impl Candidate {
    pub(super) fn of(emitted: &ToolCall, prepared: &ToolCall) -> Self {
        Self {
            names: [
                emitted.name.trim().to_owned(),
                prepared.name.trim().to_owned(),
            ],
            arguments: [&emitted.arguments_json, &prepared.arguments_json]
                .into_iter()
                .filter_map(|json| object_of(json))
                .map(|object| {
                    object
                        .into_iter()
                        .map(|(key, value)| (key, Argument::of(value)))
                        .collect()
                })
                .collect(),
        }
    }

    pub(super) fn matches(&self, block: &Block) -> bool {
        self.names.contains(&block.name)
            && self
                .arguments
                .iter()
                .any(|arguments| same_arguments(arguments, &block.arguments))
    }
}

/// One call argument with the forms a block value is compared against.
struct Argument {
    value: Value,
    /// A string's text without `\r` line ends, trimmed; anything else as
    /// compact JSON.
    text: String,
    /// `text` parsed, when it is an array or an object.
    structured: Option<Value>,
    number: Option<f64>,
}

impl Argument {
    fn of(value: Value) -> Self {
        let text = match &value {
            Value::String(text) => lf(Cow::Borrowed(text)).trim().to_owned(),
            other => other.to_string(),
        };
        Self {
            structured: structured(&text),
            number: number(&text),
            text,
            value,
        }
    }
}

fn same_arguments(call: &[(String, Argument)], block: &Map<String, Value>) -> bool {
    call.len() == block.len()
        && call
            .iter()
            .all(|(key, value)| block.get(key).is_some_and(|other| same_value(value, other)))
}

/// A block writes every value as text, and a server converts that text by
/// the parameter's type before the call arrives. A JSON block supplies typed
/// values: those compare by text both ways, as do strings.
fn same_value(call: &Argument, block: &Value) -> bool {
    match block {
        Value::String(text) if !call.value.is_string() => {
            converted(call, lf(Cow::Borrowed(text.as_str())).trim())
        }
        _ => same_text(call, block),
    }
}

/// Whether the server's conversion of the block `text` gives the typed call
/// value. The rules are Python's, as vLLM's tool parsers apply them; in
/// doubt the values count as equal, so the call is refused, not run.
fn converted(call: &Argument, text: &str) -> bool {
    match &call.value {
        Value::Bool(flag) => text.eq_ignore_ascii_case("true") == *flag,
        Value::Null => text.eq_ignore_ascii_case("null"),
        Value::Number(_) => call.number.is_some() && call.number == lenient_number(text),
        // A Python literal (`[{'a': True}]`) does not parse as JSON.
        _ if text.starts_with(['[', '{']) => {
            match agentkit_contracts::parse_javascript_json(text) {
                Ok(parsed) => call.structured.as_ref() == Some(&parsed),
                Err(_) => true,
            }
        }
        _ => false,
    }
}

/// Values compare by trimmed text: a string by its text, anything else by
/// its compact JSON. A block writes numbers, arrays and objects as free text
/// with its own spacing, so text that parses compares parsed; a server reads
/// a boolean written `True` as `true`.
fn same_text(call: &Argument, block: &Value) -> bool {
    let written = match block {
        Value::String(text) => lf(Cow::Borrowed(text.as_str())),
        other => Cow::Owned(other.to_string()),
    };
    let right = written.trim();
    if call.text == right {
        return true;
    }
    if (call.value.is_boolean() || block.is_boolean()) && call.text.eq_ignore_ascii_case(right) {
        return true;
    }
    if let Some(left) = &call.structured
        && let Some(right) = structured(right)
    {
        return *left == right;
    }
    matches!((call.number, number(right)), (Some(left), Some(right)) if left == right)
}

fn structured(text: &str) -> Option<Value> {
    if !text.starts_with(['{', '[']) {
        return None;
    }
    agentkit_contracts::parse_javascript_json(text).ok()
}

fn number(text: &str) -> Option<f64> {
    text.parse::<f64>().ok().filter(|value| value.is_finite())
}

/// `text` as Python's `int()`/`float()` read it: `_` between digits and any
/// Unicode decimal digit are accepted.
fn lenient_number(text: &str) -> Option<f64> {
    let ascii: String = text
        .chars()
        .filter(|ch| *ch != '_')
        .map(ascii_digit)
        .collect();
    number(&ascii)
}

/// The ASCII digit a Unicode decimal digit stands for; other characters as
/// they are.
fn ascii_digit(ch: char) -> char {
    if ch.is_ascii() {
        return ch;
    }
    let code = u32::from(ch);
    let run = DIGIT_ZEROS.partition_point(|zero| *zero <= code);
    run.checked_sub(1)
        .and_then(|run| char::from_digit(code - DIGIT_ZEROS[run], 10))
        .unwrap_or(ch)
}

/// The zero of every run of ten Unicode decimal digits (category Nd,
/// Unicode 17.0, from Python's `unicodedata`); each run counts 0 to 9.
const DIGIT_ZEROS: [u32; 77] = [
    0x30, 0x660, 0x6F0, 0x7C0, 0x966, 0x9E6, 0xA66, 0xAE6, 0xB66, 0xBE6, 0xC66, 0xCE6, 0xD66,
    0xDE6, 0xE50, 0xED0, 0xF20, 0x1040, 0x1090, 0x17E0, 0x1810, 0x1946, 0x19D0, 0x1A80, 0x1A90,
    0x1B50, 0x1BB0, 0x1C40, 0x1C50, 0xA620, 0xA8D0, 0xA900, 0xA9D0, 0xA9F0, 0xAA50, 0xABF0, 0xFF10,
    0x104A0, 0x10D30, 0x10D40, 0x11066, 0x110F0, 0x11136, 0x111D0, 0x112F0, 0x11450, 0x114D0,
    0x11650, 0x116C0, 0x116D0, 0x116DA, 0x11730, 0x118E0, 0x11950, 0x11BF0, 0x11C50, 0x11D50,
    0x11DA0, 0x11DE0, 0x11F50, 0x16130, 0x16A60, 0x16AC0, 0x16B50, 0x16D70, 0x1CCF0, 0x1D7CE,
    0x1D7D8, 0x1D7E2, 0x1D7EC, 0x1D7F6, 0x1E140, 0x1E2F0, 0x1E4F0, 0x1E5F1, 0x1E950, 0x1FBF0,
];
