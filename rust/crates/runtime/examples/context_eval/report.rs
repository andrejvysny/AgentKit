use super::{
    capture::{Capture, run},
    scenarios::{Scenario, baseline_scenarios},
};
use agentkit_testkit::prefix_harness::{
    EventMark, compare_recorded, exception_between, layout_violations, system_block_count,
};
use serde_json::{Value, json};

fn cache_breaks(capture: &Capture) -> Vec<EventMark> {
    capture
        .events
        .iter()
        .enumerate()
        .flat_map(|(turn, events)| {
            events
                .iter()
                .filter(|event| {
                    event["type"] == "context.cache_break"
                        || (event["type"] == "run.warning"
                            && event["data"]["code"] == "prefix_refrozen")
                })
                .map(move |event| EventMark {
                    turn,
                    seq: event["seq"].as_f64().expect("cache-break event sequence"),
                })
        })
        .collect()
}

fn source_hint(case: &Scenario, previous: usize, path: &str) -> Option<&'static str> {
    let schedule = case.config.schedule.unwrap_or_default();
    if previous == 3 && path == "/messages/3/content" {
        Some("Stale tool-result elision: crates/chat/src/chat.rs:352 and chat/elision.rs:159")
    } else if schedule.rules && previous == 7 && path == "/messages/0/content" {
        Some("Project instructions reloaded: crates/chat/src/chat.rs:278")
    } else if schedule.selection
        && previous == 11
        && (path == "/messages/0/content" || path == "/instructions")
    {
        Some("Managed selection applied: crates/chat/src/chat.rs:182,462")
    } else if schedule.tools && previous == 15 && path.starts_with("/tools/") {
        Some("Contributor visibility restaged: crates/chat/src/chat/hooks.rs:418-446")
    } else {
        None
    }
}

fn pairs(case: &Scenario, capture: &Capture, marks: &[EventMark]) -> (Vec<Value>, Vec<Value>) {
    let key = if case.config.protocol == "responses" {
        "input"
    } else {
        "messages"
    };
    let mut breaks = Vec::new();
    let mut exceptions = Vec::new();
    for (index, pair) in capture.requests.windows(2).enumerate() {
        let (old, before) = &pair[0];
        let (new, after) = &pair[1];
        if exception_between(*before, *after, marks) {
            exceptions.push(json!({"previousRequest":index + 1,"nextRequest":index + 2,
                "marks":marks.iter().filter(|mark| **mark > *before && **mark <= *after).collect::<Vec<_>>() }));
        } else if let Some(found) = compare_recorded(old, new, key, *before, *after, marks) {
            breaks.push(json!({"previousRequest":index + 1,"nextRequest":index + 2,
                "byteOffset":found.position,"path":found.path,
                "sourceHypothesis":source_hint(case, index, &found.path)}));
        }
    }
    (breaks, exceptions)
}

fn failures(capture: &Capture) -> Vec<Value> {
    capture
        .statuses
        .iter()
        .enumerate()
        .filter(|(_, status)| *status != "Completed")
        .map(|(turn, status)| {
            let events = &capture.events[turn];
            let mismatch = events.iter().any(|event| event["type"] == "run.failed" && event["data"]["errorCode"] == "continuation_history_mismatch");
            let hint = mismatch.then_some("Continuation selection and history digest guards: crates/providers/src/responses_request.rs:68,169; event does not identify which guard failed");
            json!({"turn":turn + 1,"status":status,"events":events,"sourceHint":hint})
        })
        .collect()
}

/// Per request: leading system blocks on the wire and any system item that
/// follows history (a CE-01 layout violation). Report mode only.
fn layout(case: &Scenario, capture: &Capture) -> Vec<Value> {
    let key = if case.config.protocol == "responses" {
        "input"
    } else {
        "messages"
    };
    capture
        .requests
        .iter()
        .enumerate()
        .map(|(index, (body, _))| {
            json!({"request":index + 1,
                "systemBlocks":system_block_count(body, key),
                "violations":layout_violations(body, key)})
        })
        .collect()
}

pub fn summarize(case: &Scenario, capture: &Capture, include_captures: bool) -> Value {
    let marks = cache_breaks(capture);
    let (breaks, exceptions) = pairs(case, capture, &marks);
    let mut result = json!({
        "scenario":case.name,"protocol":case.config.protocol,"profile":case.config.profile,
        "turnCount":capture.statuses.len(),"requestCount":capture.requests.len(),
        "statuses":capture.statuses,"failures":failures(capture),"cacheBreakEvents":marks,
        "breaks":breaks,"exceptions":exceptions,"layout":layout(case, capture),
    });
    if include_captures {
        result["rawCapture"] = json!({"capture":capture,"serializedBodies":capture.requests.iter()
            .map(|(body, _)| body.to_string()).collect::<Vec<_>>()});
    }
    result
}

pub async fn baseline_with_captures(include_captures: bool) -> Value {
    let mut results = Vec::new();
    for case in baseline_scenarios() {
        let capture = run(case.config.clone()).await;
        results.push(summarize(&case, &capture, include_captures));
    }
    json!({"mode":"report-only","profiles":"custom AgentKit fixtures; tool-result elision enabled",
        "capture":"provider-assembled Values, not literal HTTP wire bytes",
        "comparison":"literal provider-assembled serialization through previous last input item; zero-based UTF-8 byte offsets; source hypotheses require corroboration",
        "scope":"scripted offline sessions; not a full CE-02 or live-provider claim; Anthropic deferred",
        "scenarios":results})
}
