//! Shaped results (roadmap 2.2): the marker a tool wraps around a raw text
//! and its smaller shaped text, and the provenance check the gate applies.
use super::OutputHandle;
use serde_json::Value;

/// Keys of a shaped result (roadmap 2.2, ADR 0034 addendum). A tool that has
/// both the raw text and a smaller shaped text for the model returns
/// `model_data = shaped_value(raw, shaped, hidden, what, note)`. The gate
/// stores the raw text and shows the shaped text with a footer naming the
/// handle. A tool returns this only when the context says an output store is
/// in play (`OUTPUT_STORE_METADATA_KEY`).
///
/// Provenance: [`shaped_value`] also writes `SHAPED_PROOF_KEY`, a per-process
/// token no tool output can know. The gate takes the marker only with that
/// proof; every other result has the marker keys stripped, so a foreign tool
/// (an MCP server, a file's text) cannot hide its output behind a forged one.
pub const SHAPED_TEXT_KEY: &str = "shapedText";
pub const SHAPED_HIDDEN_KEY: &str = "shapedHidden";
pub(super) const SHAPED_PROOF_KEY: &str = "shapedProof";
/// Room the shaped footer takes besides its `what` and `note` text: count,
/// handle and the storage-cap remark.
pub(super) const SHAPED_FOOTER_ROOM: usize = 192;
/// A shaped text is used only when it, with its footer, is at most this share
/// of the raw text (the 20% bar).
pub(super) const SHAPED_MAX_SHARE_PERCENT: usize = 80;

fn shaped_proof() -> &'static str {
    static PROOF: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    PROOF.get_or_init(|| OutputHandle::generate().to_string())
}

/// The model-data value a tool returns for a shaped result: `hidden` items of
/// kind `what` (for example `43 passing tests`) are in `raw` but not `shaped`.
/// `note` is an optional remark for the footer, for example that the tool had
/// capped its own output.
pub fn shaped_value(raw: &str, shaped: &str, hidden: u64, what: &str, note: Option<&str>) -> Value {
    serde_json::json!({
        "text": raw,
        SHAPED_TEXT_KEY: shaped,
        SHAPED_HIDDEN_KEY: { "count": hidden, "what": what, "note": note },
        SHAPED_PROOF_KEY: shaped_proof(),
    })
}

/// A shaped text taken out of a result value.
#[derive(Clone)]
pub(super) struct ShapedText {
    pub(super) text: String,
    pub(super) hidden: u64,
    pub(super) what: String,
    pub(super) note: Option<String>,
}

/// Remove the marker keys from `value`, whatever their origin. Returns the
/// removed parts and whether anything was removed.
fn strip_shaped_keys(value: &mut Value) -> Option<[Option<Value>; 3]> {
    let map = value.as_object_mut()?;
    let removed = [
        map.remove(SHAPED_TEXT_KEY),
        map.remove(SHAPED_HIDDEN_KEY),
        map.remove(SHAPED_PROOF_KEY),
    ];
    removed.iter().any(Option::is_some).then_some(removed)
}

/// Strip the marker from a result that no gate will shape.
pub(super) fn strip_marker(value: &mut Value) {
    if strip_shaped_keys(value).is_some() {
        collapse_text(value);
    }
}

/// A value left with only its `text` becomes that bare string, as the pack
/// wrote it unshaped.
fn collapse_text(value: &mut Value) {
    if let Some(map) = value.as_object()
        && map.len() == 1
        && let Some(text @ Value::String(_)) = map.get("text")
    {
        *value = text.clone();
    }
}

/// Take the shaped marker out of `value`. The marker keys are always removed;
/// the shaped text is returned only when the proof matches and `text` is a
/// string. The value is left as the raw result: a bare string when `text`
/// was its only member.
pub(super) fn take_shaped(value: &mut Value) -> Option<ShapedText> {
    let [text, hidden, proof] = strip_shaped_keys(value)?;
    let proven = proof.as_ref().and_then(Value::as_str) == Some(shaped_proof());
    let text = text.as_ref().and_then(Value::as_str).map(str::to_owned);
    let map = value.as_object_mut()?;
    if !proven || !map.get("text").is_some_and(Value::is_string) {
        collapse_text(value);
        return None;
    }
    let text = text?;
    let count = hidden
        .as_ref()
        .and_then(|value| value.get("count"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let what = hidden
        .as_ref()
        .and_then(|value| value.get("what"))
        .and_then(Value::as_str)
        .unwrap_or("lines")
        .to_owned();
    let note = hidden
        .as_ref()
        .and_then(|value| value.get("note"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    if map.len() == 1 {
        let raw = map.get("text").cloned().unwrap_or_default();
        *value = raw;
    }
    Some(ShapedText {
        text,
        hidden: count,
        what,
        note,
    })
}
