//! Tool-call blocks in one text, read as leniently as a server reads them.

use super::Block;
use crate::model_profile::salvage_tool_calls;
use serde_json::{Map, Value};

const CALL_OPEN: &str = "<tool_call>";
const CALL_CLOSE: &str = "</tool_call>";
const FUNCTION_OPEN: &str = "<function=";
const FUNCTION_CLOSE: &str = "</function>";
const PARAMETER_OPEN: &str = "<parameter=";
const PARAMETER_CLOSE: &str = "</parameter>";

/// Every block in `text`, by the guard's own parsers and by the loop's
/// salvage parser: they end a parameter value at different tags, and a
/// value holding the other parser's tag must match either way.
pub(super) fn blocks(text: &str, on_block: &mut dyn FnMut(Block)) {
    lenient_blocks(text, on_block);
    salvaged(text, on_block);
}

/// The blocks the guard's own parsers find. A value ends at the next
/// `<parameter=` as well as at `</parameter>`, as vLLM reads it.
pub(super) fn lenient_blocks(text: &str, on_block: &mut dyn FnMut(Block)) {
    regions(
        text,
        FUNCTION_OPEN,
        &[FUNCTION_CLOSE, CALL_CLOSE],
        parse_function,
        on_block,
    );
    regions(text, CALL_OPEN, &[CALL_CLOSE], json_block, on_block);
}

/// The calls `salvage_tool_calls` finds in `text`, read piece by piece: a
/// block there ends at the next `</tool_call>`, or, when none is left, at
/// the next `</function>`, so these pieces give the same calls as the whole.
/// Read whole, every block searches the rest of the text for a closer that
/// is gone, which is quadratic in hostile text.
pub(super) fn salvaged(text: &str, on_block: &mut dyn FnMut(Block)) {
    if !text.contains(CALL_OPEN) {
        return;
    }
    let (closed, open) = text
        .rfind(CALL_CLOSE)
        .map_or(("", text), |at| text.split_at(at + CALL_CLOSE.len()));
    let pieces = closed
        .split_inclusive(CALL_CLOSE)
        .chain(open.split_inclusive(FUNCTION_CLOSE));
    for piece in pieces {
        for call in salvage_tool_calls(piece).map_or_else(Vec::new, |found| found.calls) {
            on_block(Block {
                name: call.name.trim().to_owned(),
                arguments: trimmed_keys(call.arguments),
            });
        }
    }
}

/// Forward-only positions of one tag. Positions asked for never decrease, so
/// each search starts after the previous result and a missing tag is never
/// searched for again.
struct Tag<'t> {
    text: &'t str,
    tag: &'static str,
    found: Option<usize>,
    gone: bool,
}

impl<'t> Tag<'t> {
    fn new(text: &'t str, tag: &'static str) -> Self {
        Self {
            text,
            tag,
            found: None,
            gone: false,
        }
    }

    fn at_or_after(&mut self, at: usize) -> Option<usize> {
        if let Some(found) = self.found
            && found >= at
        {
            return Some(found);
        }
        if self.gone {
            return None;
        }
        self.found = self
            .text
            .get(at..)
            .and_then(|rest| rest.find(self.tag))
            .map(|offset| at + offset);
        self.gone = self.found.is_none();
        self.found
    }
}

/// Each region from an `open` tag to the first following `ends` tag (or the
/// end of the text), left to right without overlap as servers match blocks.
/// A region is parsed from its first opener and, when another opener sits
/// inside it, from the last one too: parsers disagree on which one counts.
fn regions(
    text: &str,
    open: &'static str,
    ends: &[&'static str],
    parse: fn(&str) -> Option<Block>,
    on_block: &mut dyn FnMut(Block),
) {
    let mut opens = Tag::new(text, open);
    let mut ends: Vec<Tag<'_>> = ends.iter().map(|tag| Tag::new(text, tag)).collect();
    let mut at = 0;
    while let Some(first) = opens.at_or_after(at) {
        let start = first + open.len();
        let end = ends
            .iter_mut()
            .filter_map(|tag| tag.at_or_after(start))
            .min()
            .unwrap_or(text.len());
        let mut last = first;
        while let Some(next) = opens.at_or_after(last + open.len())
            && next < end
        {
            last = next;
        }
        if let Some(block) = parse(&text[start..end]) {
            on_block(block);
        }
        if last != first
            && let Some(block) = parse(&text[last + open.len()..end])
        {
            on_block(block);
        }
        at = end;
    }
}

/// `name>` then parameters, the text after `<function=`.
fn parse_function(region: &str) -> Option<Block> {
    let close = region.find('>')?;
    let name = region[..close].trim();
    if name.is_empty() {
        return None;
    }
    Some(Block {
        name: name.to_owned(),
        arguments: parameters(&region[close + 1..]),
    })
}

/// Every `<parameter=key>value` in `body`. A value ends at its closing tag,
/// the next parameter or the end of the body; text between parameters is
/// ignored, as a lenient server ignores it.
fn parameters(body: &str) -> Map<String, Value> {
    let mut opens = Tag::new(body, PARAMETER_OPEN);
    let mut closes = Tag::new(body, PARAMETER_CLOSE);
    let mut arguments = Map::new();
    let mut at = 0;
    while let Some(open) = opens.at_or_after(at) {
        let start = open + PARAMETER_OPEN.len();
        let close = closes.at_or_after(start);
        let end = [close, opens.at_or_after(start)]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(body.len());
        if let Some(key_end) = body[start..end].find('>') {
            let key = body[start..start + key_end].trim();
            if !key.is_empty() {
                let value = &body[start + key_end + 1..end];
                arguments.insert(key.to_owned(), Value::String(value.to_owned()));
            }
        }
        at = if close == Some(end) {
            end + PARAMETER_CLOSE.len()
        } else {
            end
        };
    }
    arguments
}

/// `{"name": ..., "arguments": {...}}`, the text after `<tool_call>`.
fn json_block(region: &str) -> Option<Block> {
    let region = region.trim();
    if !region.starts_with('{') {
        return None;
    }
    let Value::Object(mut object) = leading_json(region)? else {
        return None;
    };
    let name = object.get("name")?.as_str()?.trim().to_owned();
    let arguments = match object
        .remove("arguments")
        .or_else(|| object.remove("parameters"))
    {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(arguments)) => trimmed_keys(arguments),
        Some(Value::String(text)) => object_of(&text)?,
        Some(_) => return None,
    };
    (!name.is_empty()).then_some(Block { name, arguments })
}

/// The JSON value `text` starts with. A block whose `</tool_call>` is
/// missing runs on into whatever follows it, which is ignored.
fn leading_json(text: &str) -> Option<Value> {
    agentkit_contracts::parse_javascript_json(text)
        .ok()
        .or_else(|| {
            let mut values = serde_json::Deserializer::from_str(text)
                .into_iter::<Box<serde_json::value::RawValue>>();
            let raw = values.next()?.ok()?;
            agentkit_contracts::parse_javascript_json(raw.get()).ok()
        })
}

/// Arguments written as JSON text: an object, or none at all.
pub(super) fn object_of(text: &str) -> Option<Map<String, Value>> {
    let text = text.trim();
    if text.is_empty() {
        return Some(Map::new());
    }
    match agentkit_contracts::parse_javascript_json(text).ok()? {
        Value::Object(object) => Some(trimmed_keys(object)),
        Value::Null => Some(Map::new()),
        _ => None,
    }
}

pub(super) fn trimmed_keys(object: Map<String, Value>) -> Map<String, Value> {
    object
        .into_iter()
        .map(|(key, value)| (key.trim().to_owned(), value))
        .collect()
}
