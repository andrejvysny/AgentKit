use serde_json::{Value, json};

pub fn validate_requests(expected: &Value, actual: &[Value]) -> Result<(), String> {
    let requests = expected["requests"].as_array().ok_or("missing requests")?;
    if requests.len() != actual.len() {
        return Err("request count differs".into());
    }
    for (index, (expected, actual)) in requests.iter().zip(actual).enumerate() {
        // Value equality ignores object order, which changes cache prefix bytes.
        let recorded =
            serde_json::to_vec(&expected["request"]).map_err(|error| error.to_string())?;
        let assembled = serde_json::to_vec(actual).map_err(|error| error.to_string())?;
        if recorded != assembled {
            return Err(format!("request {index} differs"));
        }
    }
    Ok(())
}

fn validate_frame(frame: &Value) -> Result<(), String> {
    for path in [
        "/choices",
        "/choices/0/delta/tool_calls",
        "/response/output",
        "/response/output/0/content",
    ] {
        if frame
            .pointer(path)
            .and_then(Value::as_array)
            .is_some_and(|a| a.len() != 1)
        {
            return Err("unsupported multi-part output frame".into());
        }
    }
    if frame.pointer("/choices/0/delta/tool_calls").is_some()
        && frame
            .pointer("/choices/0/delta/content")
            .and_then(Value::as_str)
            .is_some()
    {
        return Err("unsupported mixed output frame".into());
    }

    if frame.pointer("/response/output/0/arguments").is_some()
        && frame.pointer("/response/output/0/content").is_some()
    {
        return Err("unsupported mixed output frame".into());
    }
    Ok(())
}

pub fn frame_output(frame: &Value) -> Result<Value, String> {
    validate_frame(frame)?;
    let tool = frame
        .pointer("/choices/0/delta/tool_calls/0/function/arguments")
        .or_else(|| frame.pointer("/response/output/0/arguments"));
    let text = frame
        .pointer("/choices/0/delta/content")
        .or_else(|| frame.pointer("/response/output/0/content/0/text"));
    if let Some(raw) = tool.and_then(Value::as_str) {
        return Ok(json!({"kind":"tool","value":raw}));
    }
    if let Some(raw) = text.and_then(Value::as_str) {
        return Ok(json!({"kind":"text","value":raw}));
    }
    Err("unsupported output frame".into())
}

pub fn event_output(frame: &Value, events: &[Value]) -> Result<Value, String> {
    let expected = frame_output(frame)?;
    let key = if expected["kind"] == "tool" {
        "argumentsJson"
    } else {
        "content"
    };
    let event_type = if expected["kind"] == "tool" {
        "run.tool.requested"
    } else {
        "run.message.completed"
    };
    let matched = events
        .iter()
        .any(|event| event["type"] == event_type && event["data"][key] == expected["value"]);
    if matched {
        Ok(expected)
    } else {
        Err("durable event does not match frame output".into())
    }
}
