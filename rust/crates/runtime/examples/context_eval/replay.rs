use super::capture::{CaptureConfig, run};
use super::replay_validation::{event_output, validate_requests};
use serde_json::Value;
use std::fs;

fn config(session: &Value) -> CaptureConfig {
    let requests = session["requests"].as_array().expect("requests");
    let frames = requests
        .iter()
        .map(|r| format!("data: {}\n\ndata: [DONE]\n\n", r["response_frame"]))
        .collect();
    let tool_results = session["scripted_tool_results"]
        .as_array()
        .expect("tool results")
        .iter()
        .map(|v| v["text"].as_str().expect("tool result text").to_owned())
        .collect();
    let inputs = session["scripted_inputs"]
        .as_array()
        .expect("inputs")
        .iter()
        .map(|v| v.as_str().expect("input text").to_owned())
        .collect();
    let protocol = if requests[0]["response_frame"].get("choices").is_some() {
        "chat-completions"
    } else {
        "responses"
    };
    let profile = match session["kind"].as_str() {
        Some("chat") => "chat",
        Some("code") => "coding",
        _ => panic!("unsupported session kind"),
    };
    CaptureConfig {
        protocol: protocol.into(),
        profile: profile.into(),
        inputs,
        frames,
        tool_results,
        mutations: false,
        schedule: None,
    }
}

pub async fn verify(session: &Value) -> Result<(), String> {
    if session["outcome"] != "script completed" {
        return Err("outcome must describe scripted completion only".into());
    }
    let requests = session["requests"].as_array().ok_or("missing requests")?;
    if requests
        .iter()
        .any(|request| request.get("follow_handles") != Some(&Value::Null))
    {
        return Err("follow_handles must remain unknown without instrumentation".into());
    }
    let configuration = config(session);
    let expected_turns = configuration.inputs.len();
    if expected_turns == 0 {
        return Err("scripted inputs must not be empty".into());
    }
    let capture = run(configuration).await;
    if capture.statuses.len() != expected_turns || capture.statuses.iter().any(|s| s != "Completed")
    {
        return Err(format!("unexpected statuses: {:?}", capture.statuses));
    }
    let values: Vec<_> = capture
        .requests
        .iter()
        .map(|(value, _)| value.clone())
        .collect();
    validate_requests(session, &values)?;
    validate_outputs(session, &capture)
}

fn validate_outputs(session: &Value, capture: &super::capture::Capture) -> Result<(), String> {
    let requests = session["requests"].as_array().ok_or("missing requests")?;
    for (request, (_, mark)) in requests.iter().zip(&capture.requests) {
        let events = capture
            .events
            .get(mark.turn)
            .ok_or("missing durable events")?;
        let evidence = event_output(&request["response_frame"], events)?;
        if request["response"]["content"] != evidence["value"] {
            return Err("response does not match frame".into());
        }
        if request["output_evidence"] != evidence {
            return Err("durable output evidence differs".into());
        }
    }
    Ok(())
}

/// Validate a frozen corpus without rewriting its input or output evidence.
pub async fn verify_file(input: &str) -> Result<usize, String> {
    let before = fs::read(input).map_err(|error| error.to_string())?;
    let source: Vec<Value> = serde_json::from_slice(&before).map_err(|error| error.to_string())?;
    if source.is_empty() {
        return Err("expected nonempty session array".into());
    }
    for session in &source {
        verify(session).await?;
    }
    if before != fs::read(input).map_err(|error| error.to_string())? {
        return Err("fixture changed during replay".into());
    }
    Ok(source.len())
}
