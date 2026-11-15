use super::client::{Endpoint, Reply};
use super::report::{CheckResult, Report, Status};
use serde_json::{Value, json};

use super::checks_more as more;

/// (id, title, what the answer decides) — the spec's probe table.
const TABLE: [(&str, &str, &str); 18] = [
    ("P1", "one tool call round trip", "parser is configured"),
    (
        "P2",
        "second turn with call in history",
        "G6: arguments as JSON text in history",
    ),
    (
        "P3",
        "two calls in one reply",
        "parallel calls, distinct ids",
    ),
    ("P4", "streamed tool call", "stream handling"),
    ("P5", "reasoning returned", "TU-17: reasoning field name"),
    (
        "P6",
        "template options pass through",
        "G2: thinking off, effort low",
    ),
    ("P7", "effort field high", "G3"),
    ("P8", "extra sampling fields", "G2"),
    (
        "P9",
        "long text argument verbatim",
        "whether file tools are safe",
    ),
    (
        "P10",
        "argument types",
        "how much type conversion is needed",
    ),
    (
        "P11",
        "required-call reliability",
        "the reported one-in-ten failure",
    ),
    (
        "P12",
        "second system message",
        "G5, G7: error text and status",
    ),
    ("P13", "images in user and tool messages", "image policy"),
    ("P14", "model list and model info", "G4"),
    ("P15", "usage fields", "usage record from roadmap 0.1"),
    (
        "P16",
        "time to first token, long prefix",
        "G1: cost of promote",
    ),
    ("P17", "forced tool choice", "whether it can ever be used"),
    ("P18", "Slovak task and arguments", "first Slovak signal"),
];
pub const CHECK_IDS: [&str; 18] = [
    "P1", "P2", "P3", "P4", "P5", "P6", "P7", "P8", "P9", "P10", "P11", "P12", "P13", "P14", "P15",
    "P16", "P17", "P18",
];

pub(super) fn base(id: &str) -> CheckResult {
    let (id, title, decides) = TABLE
        .iter()
        .find(|row| row.0 == id)
        .copied()
        .unwrap_or(("?", "", ""));
    CheckResult::new(id, title, decides)
}
pub(super) fn pass(id: &str, detail: impl Into<String>) -> CheckResult {
    base(id).with(Status::Pass, detail)
}
pub(super) fn fail(id: &str, detail: impl Into<String>) -> CheckResult {
    base(id).with(Status::Fail, detail)
}
pub(super) fn info(id: &str, detail: impl Into<String>) -> CheckResult {
    base(id).with(Status::Info, detail)
}
pub(super) fn error(id: &str, detail: impl Into<String>) -> CheckResult {
    base(id).with(Status::Error, detail)
}
pub(super) fn http_fail(id: &str, reply: &Reply) -> CheckResult {
    fail(
        id,
        format!("HTTP {}: {}", reply.status, truncate(&reply.text, 300)),
    )
}
pub(super) fn truncate(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

pub(super) fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({"type":"function","function":{"name":name,"description":description,
        "parameters":{"type":"object","properties":properties,"required":required}}})
}
pub(super) fn weather_tool() -> Value {
    tool(
        "get_weather",
        "Current weather for a city.",
        json!({"city":{"type":"string","description":"City name"}}),
        &["city"],
    )
}
pub(super) fn user(text: &str) -> Value {
    json!({"role":"user","content":text})
}
pub(super) fn tool_calls(message: &Value) -> Vec<Value> {
    message["tool_calls"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}
/// Arguments of a call, parsed from the JSON text the API requires.
pub(super) fn args(call: &Value) -> Option<Value> {
    serde_json::from_str(call["function"]["arguments"].as_str()?).ok()
}
pub(super) fn reasoning_field(message: &Value) -> Option<&'static str> {
    ["reasoning_content", "reasoning"]
        .into_iter()
        .find(|name| message[*name].as_str().is_some_and(|text| !text.is_empty()))
}

pub async fn run_checks(endpoint: &Endpoint, only: &[String], runs: usize) -> Report {
    let mut results = Vec::new();
    for id in CHECK_IDS {
        if !only.is_empty() && !only.iter().any(|wanted| wanted == id) {
            continue;
        }
        results.push(match id {
            "P1" => p1(endpoint).await,
            "P2" => p2(endpoint).await,
            "P3" => p3(endpoint).await,
            "P4" => p4(endpoint).await,
            "P5" => more::p5(endpoint).await,
            "P6" => more::p6(endpoint).await,
            "P7" => more::p7(endpoint).await,
            "P8" => more::p8(endpoint).await,
            "P9" => p9(endpoint).await,
            "P10" => p10(endpoint).await,
            "P11" => more::p11(endpoint, runs).await,
            "P12" => more::p12(endpoint).await,
            "P13" => more::p13(endpoint).await,
            "P14" => more::p14(endpoint).await,
            "P15" => more::p15(endpoint).await,
            "P16" => more::p16(endpoint).await,
            "P17" => more::p17(endpoint).await,
            _ => more::p18(endpoint).await,
        });
    }
    Report {
        base_url: endpoint.base_url.clone(),
        model: endpoint.model.clone(),
        results,
        secret: endpoint.key.clone(),
    }
}

async fn p1(ep: &Endpoint) -> CheckResult {
    let body = ep.chat_body(
        json!([user("What is the weather in Paris? Use the tool.")]),
        json!({"tools":[weather_tool()],"tool_choice":"auto"}),
    );
    let reply = match ep.chat(&body).await {
        Ok(reply) => reply,
        Err(e) => return error("P1", e),
    };
    if !reply.ok() {
        return http_fail("P1", &reply);
    }
    let calls = tool_calls(reply.message());
    match calls
        .first()
        .map(|c| (c["function"]["name"].as_str(), args(c)))
    {
        Some((Some("get_weather"), Some(_))) => pass("P1", "one valid get_weather call"),
        Some(_) => fail("P1", "call present but name or arguments invalid"),
        None => fail(
            "P1",
            format!("no tool call; content: {}", truncate(&reply.text, 200)),
        ),
    }
}

pub(super) fn history_with_call() -> Value {
    json!([
        user("What is the weather in Paris? Use the tool."),
        {"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function",
            "function":{"name":"get_weather","arguments":"{\"city\":\"Paris\"}"}}]},
        {"role":"tool","tool_call_id":"call_1","content":"{\"temp_c\":18}"}
    ])
}

async fn p2(ep: &Endpoint) -> CheckResult {
    let body = ep.chat_body(history_with_call(), json!({"tools":[weather_tool()]}));
    match ep.chat(&body).await {
        Err(e) => error("P2", e),
        Ok(r) if r.ok() => pass(
            "P2",
            "second turn accepted; history arguments read as JSON text",
        ),
        Ok(r) => http_fail("P2", &r),
    }
}

async fn p3(ep: &Endpoint) -> CheckResult {
    let body = ep.chat_body(
        json!([user(
            "Weather in Paris and in Rome. Call the tool twice, once per city, in one reply."
        )]),
        json!({"tools":[weather_tool()],"tool_choice":"auto","parallel_tool_calls":true}),
    );
    let reply = match ep.chat(&body).await {
        Ok(reply) => reply,
        Err(e) => return error("P3", e),
    };
    if !reply.ok() {
        return http_fail("P3", &reply);
    }
    let calls = tool_calls(reply.message());
    let mut ids: Vec<&str> = calls.iter().filter_map(|c| c["id"].as_str()).collect();
    ids.sort_unstable();
    ids.dedup();
    if calls.len() >= 2 && ids.len() == calls.len() {
        pass("P3", format!("{} calls, distinct ids", calls.len()))
    } else {
        fail(
            "P3",
            format!("{} calls, {} distinct ids", calls.len(), ids.len()),
        )
    }
}

/// Reassembled streamed calls as (id, name, arguments text) by index.
pub(super) fn assemble(chunks: &[Value]) -> Vec<(String, String, String)> {
    let mut calls: Vec<(String, String, String)> = Vec::new();
    for delta in chunks
        .iter()
        .filter_map(|c| c["choices"][0]["delta"]["tool_calls"].as_array())
        .flatten()
    {
        let index = delta["index"].as_u64().unwrap_or(0) as usize;
        if calls.len() <= index {
            calls.resize(index + 1, Default::default());
        }
        let slot = &mut calls[index];
        if let Some(id) = delta["id"].as_str() {
            slot.0 = id.into();
        }
        if let Some(name) = delta["function"]["name"].as_str() {
            slot.1.push_str(name);
        }
        if let Some(part) = delta["function"]["arguments"].as_str() {
            slot.2.push_str(part);
        }
    }
    calls
}

async fn p4(ep: &Endpoint) -> CheckResult {
    let body = ep.chat_body(
        json!([user("What is the weather in Paris? Use the tool.")]),
        json!({"tools":[weather_tool()],"tool_choice":"auto"}),
    );
    let streamed = match ep.chat_stream(&body).await {
        Ok(s) => s,
        Err(e) => return error("P4", e),
    };
    if !(200..300).contains(&streamed.status) {
        return fail(
            "P4",
            format!(
                "HTTP {}: {}",
                streamed.status,
                truncate(&streamed.text, 300)
            ),
        );
    }
    let calls = assemble(&streamed.chunks);
    match calls.first() {
        Some((id, name, text))
            if !id.is_empty()
                && !name.is_empty()
                && serde_json::from_str::<Value>(text).is_ok() =>
        {
            pass("P4", "id, name and valid assembled arguments")
        }
        Some((id, name, _)) => fail(
            "P4",
            format!("incomplete call: id='{id}' name='{name}' or arguments invalid"),
        ),
        None => fail("P4", "no streamed tool call"),
    }
}

pub(super) fn write_tool() -> Value {
    tool(
        "write_file",
        "Write text to a file.",
        json!({"path":{"type":"string"},"content":{"type":"string"}}),
        &["path", "content"],
    )
}

/// 200 lines with quotes, backslashes, tag-like text and Slovak diacritics.
pub(super) fn tricky_text() -> String {
    (1..=200)
        .map(|n| format!("riadok {n}: \"citát\" C:\\cesta\\{n} <tool_call>{{\"x\":{n}}}</tool_call> Ľúbozvučný ďateľ žĺtne"))
        .collect::<Vec<_>>()
        .join("\n")
}

async fn p9(ep: &Endpoint) -> CheckResult {
    let text = tricky_text();
    let body = ep.chat_body(
        json!([user(&format!("Call write_file with path notes.txt and content EXACTLY the text between the markers, byte for byte.\n<<<\n{text}\n>>>"))]),
        json!({"tools":[write_tool()],"tool_choice":"auto"}),
    );
    let reply = match ep.chat(&body).await {
        Ok(reply) => reply,
        Err(e) => return error("P9", e),
    };
    if !reply.ok() {
        return http_fail("P9", &reply);
    }
    let Some(call) = tool_calls(reply.message()).into_iter().next() else {
        return fail("P9", "no tool call");
    };
    match args(&call).and_then(|a| a["content"].as_str().map(str::to_owned)) {
        Some(got) if got == text => pass("P9", "200 lines returned byte for byte"),
        Some(got) => fail(
            "P9",
            format!("content differs ({} vs {} bytes)", got.len(), text.len()),
        ),
        None => fail("P9", "arguments invalid or content missing"),
    }
}

async fn p10(ep: &Endpoint) -> CheckResult {
    let typed = tool(
        "set_options",
        "Set options.",
        json!({"count":{"type":"integer"},"verbose":{"type":"boolean"},
            "tags":{"type":"array","items":{"type":"string"}},
            "range":{"type":"object","properties":{"from":{"type":"integer"},"to":{"type":"integer"}}}}),
        &["count", "verbose", "tags", "range"],
    );
    let body = ep.chat_body(
        json!([user(
            "Call set_options with count 3, verbose true, tags [\"a\",\"b\"], range from 1 to 5."
        )]),
        json!({"tools":[typed],"tool_choice":"auto"}),
    );
    let reply = match ep.chat(&body).await {
        Ok(reply) => reply,
        Err(e) => return error("P10", e),
    };
    if !reply.ok() {
        return http_fail("P10", &reply);
    }
    let Some(a) = tool_calls(reply.message()).first().and_then(args) else {
        return fail("P10", "no valid call");
    };
    let wrong: Vec<&str> = [
        ("count", a["count"].is_i64() || a["count"].is_u64()),
        ("verbose", a["verbose"].is_boolean()),
        ("tags", a["tags"].is_array()),
        ("range", a["range"].is_object()),
    ]
    .into_iter()
    .filter(|(_, ok)| !ok)
    .map(|(name, _)| name)
    .collect();
    if wrong.is_empty() {
        pass("P10", "integer, boolean, list and object arrived typed")
    } else {
        fail("P10", format!("wrong type: {}", wrong.join(", ")))
    }
}
