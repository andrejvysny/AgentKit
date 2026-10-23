use crate::{ToolLimits, ToolResult};
use serde_json::{json, Value};
const MARKER: &str = "\n[...truncated]";
pub(crate) fn stringify(value: &Value) -> String {
    agentkit_contracts::stringify_javascript_value(value).expect("JSON values serialize")
}
pub fn truncate(value: &str, max: usize) -> (String, bool) {
    if value.len() <= max {
        return (value.into(), false);
    }
    if max <= MARKER.len() {
        return (MARKER[..max].into(), true);
    }
    let mut end = max - MARKER.len();
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    (format!("{}{MARKER}", &value[..end]), true)
}
pub fn text_budget(max: usize) -> usize {
    128.max(max / 8)
}
pub fn error_envelope(code: &str, message: &str, detail: Value) -> Value {
    let mut data = json!({"errorCode": code, "errorMessage": message});
    if let Some(detail) = detail.as_object() {
        data.as_object_mut().expect("object").extend(detail.clone());
    }
    json!({"ok":false,"status":"error","summary":message,"warnings":[],"truncated":false,"data":data})
}
pub fn build(result: &ToolResult, limits: &ToolLimits) -> Value {
    let status = if result.status.as_deref() == Some("partial") {
        "partial"
    } else if result.ok {
        "ok"
    } else {
        "error"
    };
    let mut budget = text_budget(limits.max_bytes);
    let mut cut = result.truncated;
    let warnings: Vec<String> = result
        .warnings
        .iter()
        .filter_map(|warning| {
            if budget == 0 {
                cut = true;
                return None;
            }
            let (text, truncated) = truncate(warning, budget);
            budget -= text.len();
            cut |= truncated;
            Some(text)
        })
        .collect();
    let data = result
        .model_data
        .as_ref()
        .filter(|value| !value.is_null())
        .or(result.data.as_ref());
    let mut envelope = json!({"ok":result.ok,"status":status,"warnings":warnings,"truncated":cut});
    if let Some(data) = data {
        envelope["data"] = data.clone();
    }
    if let Some(summary) = &result.summary {
        let (summary, truncated) = truncate(summary, text_budget(limits.max_bytes));
        envelope["summary"] = summary.into();
        cut |= truncated;
        envelope["truncated"] = cut.into();
    }
    fit(envelope, limits.max_bytes)
}
fn fits(value: &Value, max: usize) -> bool {
    stringify(value).len() <= max
}
fn fit(mut envelope: Value, max: usize) -> Value {
    if fits(&envelope, max) {
        return envelope;
    }
    let data = envelope["data"].clone();
    let mut shell = envelope.clone();
    shell["data"] = Value::Null;
    let mut budget = 256.max(max.saturating_sub(stringify(&shell).len()));
    loop {
        envelope["truncated"] = true.into();
        envelope["data"] = if let Some(text) = data.as_str() {
            truncate(text, budget).0.into()
        } else {
            json!({"truncated":true,"preview":truncate(&stringify(&data),budget).0})
        };
        if fits(&envelope, max) {
            return envelope;
        }
        if budget <= 256 {
            break;
        }
        budget = 256.max(budget / 2);
    }
    envelope["data"] = json!({"truncated":true});
    if fits(&envelope, max) {
        return envelope;
    }
    envelope["warnings"] = json!([]);
    for divisor in [4, 16, usize::MAX] {
        if let Some(text) = envelope["summary"].as_str() {
            envelope["summary"] = truncate(text, max / divisor).0.into();
        }
        if fits(&envelope, max) {
            break;
        }
    }
    envelope
}
