use super::checks::{
    error, fail, history_with_call, http_fail, info, pass, reasoning_field, tool, tool_calls,
    truncate, user, weather_tool, write_tool,
};
use crate::probe::client::Endpoint;
use crate::probe::report::CheckResult;
use serde_json::{Value, json};

fn ask() -> Value {
    json!([user("What is 17 times 23? Think it through, then answer.")])
}

pub(super) async fn p5(ep: &Endpoint) -> CheckResult {
    let body = ep.chat_body(ask(), json!({}));
    let plain = match ep.chat(&body).await {
        Ok(r) => r,
        Err(e) => return error("P5", e),
    };
    if !plain.ok() {
        return http_fail("P5", &plain);
    }
    let streamed = match ep.chat_stream(&body).await {
        Ok(s) => s,
        Err(e) => return error("P5", e),
    };
    let plain_field = reasoning_field(plain.message());
    let stream_field = streamed.chunks.iter().find_map(|c| {
        ["reasoning_content", "reasoning"].into_iter().find(|name| {
            c["choices"][0]["delta"][*name]
                .as_str()
                .is_some_and(|t| !t.is_empty())
        })
    });
    let data = json!({"non_streaming": plain_field, "streaming": stream_field});
    match (plain_field, stream_field) {
        (Some(a), Some(b)) => pass(
            "P5",
            format!("reasoning in `{a}` (plain) and `{b}` (stream)"),
        ),
        (a, b) => fail("P5", format!("reasoning missing: plain={a:?} stream={b:?}")),
    }
    .data(data)
}

pub(super) async fn p6(ep: &Endpoint) -> CheckResult {
    let off = ep.chat_body(
        ask(),
        json!({"chat_template_kwargs":{"enable_thinking":false}}),
    );
    let reply = match ep.chat(&off).await {
        Ok(r) => r,
        Err(e) => return error("P6", e),
    };
    if !reply.ok() {
        return http_fail("P6", &reply);
    }
    if let Some(field) = reasoning_field(reply.message()) {
        return fail("P6", format!("thinking off still returned `{field}`"));
    }
    let low = ep.chat_body(ask(), json!({"reasoning_effort":"low"}));
    match ep.chat(&low).await {
        Ok(r) if r.ok() => pass("P6", "thinking off gives no reasoning; effort low accepted"),
        Ok(r) => http_fail("P6", &r),
        Err(e) => error("P6", e),
    }
}

pub(super) async fn p7(ep: &Endpoint) -> CheckResult {
    let body = ep.chat_body(ask(), json!({"reasoning_effort":"high"}));
    match ep.chat(&body).await {
        Ok(r) if r.ok() => pass("P7", "reasoning_effort high accepted"),
        Ok(r) => http_fail("P7", &r),
        Err(e) => error("P7", e),
    }
}

pub(super) async fn p8(ep: &Endpoint) -> CheckResult {
    let body = ep.chat_body(
        ask(),
        json!({"top_k":20,"min_p":0.0,"repetition_penalty":1.0}),
    );
    match ep.chat(&body).await {
        Ok(r) if r.ok() => pass("P8", "top_k, min_p, repetition_penalty accepted"),
        Ok(r) => http_fail("P8", &r),
        Err(e) => error("P8", e),
    }
}

/// Tool-call markup the server failed to parse, left in text.
fn markup_in_text(message: &Value) -> bool {
    ["content", "reasoning_content", "reasoning"]
        .iter()
        .any(|key| {
            message[*key]
                .as_str()
                .is_some_and(|t| t.contains("<tool_call>") || t.contains("<function="))
        })
}

pub(super) async fn p11(ep: &Endpoint, runs: usize) -> CheckResult {
    let body = ep.chat_body(
        json!([user("What is the weather in Paris? Use the tool.")]),
        json!({"tools":[weather_tool()],"tool_choice":"required"}),
    );
    let (mut none, mut malformed, mut in_text, mut errors) = (0, 0, 0, 0);
    for _ in 0..runs {
        let Ok(reply) = ep.chat(&body).await else {
            errors += 1;
            continue;
        };
        if !reply.ok() {
            errors += 1;
            continue;
        }
        let calls = tool_calls(reply.message());
        if calls.is_empty() {
            if markup_in_text(reply.message()) {
                in_text += 1
            } else {
                none += 1
            }
        } else if calls.iter().any(|c| super::checks::args(c).is_none()) {
            malformed += 1;
        }
    }
    let bad = none + malformed + in_text + errors;
    let data = json!({"runs":runs,"no_call":none,"malformed":malformed,"call_in_text":in_text,"errors":errors});
    let detail = format!(
        "{bad}/{runs} failed: no_call={none} malformed={malformed} in_text={in_text} errors={errors}"
    );
    // A one-in-ten rate is the failure this check exists to find; 5% is the line.
    if runs > 0 && bad * 20 <= runs {
        pass("P11", detail)
    } else {
        fail("P11", detail)
    }
    .data(data)
}

pub(super) async fn p12(ep: &Endpoint) -> CheckResult {
    let body = ep.chat_body(
        json!([{"role":"system","content":"First."},{"role":"system","content":"Second."},user("Hi")]),
        json!({}),
    );
    match ep.chat(&body).await {
        Ok(r) if r.ok() => info("P12", "second system message accepted"),
        Ok(r) => info(
            "P12",
            format!("rejected: HTTP {} {}", r.status, truncate(&r.text, 300)),
        ),
        Err(e) => error("P12", e),
    }
}

const PIXEL: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==";

pub(super) async fn p13(ep: &Endpoint) -> CheckResult {
    let in_user = ep.chat_body(
        json!([{"role":"user","content":[{"type":"text","text":"Describe the image."},
            {"type":"image_url","image_url":{"url":PIXEL}}]}]),
        json!({}),
    );
    let mut history = history_with_call();
    history[2]["content"] =
        json!([{"type":"text","text":"chart"},{"type":"image_url","image_url":{"url":PIXEL}}]);
    let in_tool = ep.chat_body(history, json!({"tools":[weather_tool()]}));
    let (mut parts, mut data) = (Vec::new(), json!({}));
    for (name, body) in [("user", in_user), ("tool_result", in_tool)] {
        match ep.chat(&body).await {
            Ok(r) => {
                parts.push(format!("{name}: HTTP {}", r.status));
                data[name] = r.status.into();
            }
            Err(e) => return error("P13", e),
        }
    }
    info("P13", parts.join("; ")).data(data)
}

pub(super) async fn p14(ep: &Endpoint) -> CheckResult {
    let models = match ep.get("/models").await {
        Ok(r) => r,
        Err(e) => return error("P14", e),
    };
    if !models.ok() {
        return http_fail("P14", &models);
    }
    let listed = models.body["data"].as_array().map_or(0, Vec::len);
    let has_context = models.body["data"].as_array().is_some_and(|items| {
        items
            .iter()
            .any(|m| m["max_model_len"].is_number() || m["context_length"].is_number())
    });
    // LiteLLM proxy model info; absent on plain vLLM.
    let proxy = ep.get("/model/info").await.map(|r| r.ok()).unwrap_or(false);
    let data = json!({"models":listed,"context_in_list":has_context,"proxy_model_info":proxy});
    if listed > 0 {
        pass(
            "P14",
            format!("{listed} models; context in list: {has_context}; proxy model info: {proxy}"),
        )
    } else {
        fail("P14", "model list empty")
    }
    .data(data)
}

pub(super) async fn p15(ep: &Endpoint) -> CheckResult {
    let reply = match ep.chat(&ep.chat_body(ask(), json!({}))).await {
        Ok(r) => r,
        Err(e) => return error("P15", e),
    };
    if !reply.ok() {
        return http_fail("P15", &reply);
    }
    let u = &reply.body["usage"];
    let present = json!({
        "prompt_tokens": u["prompt_tokens"].is_number(),
        "completion_tokens": u["completion_tokens"].is_number(),
        "cached_tokens": u["prompt_tokens_details"]["cached_tokens"].is_number(),
        "reasoning_tokens": u["completion_tokens_details"]["reasoning_tokens"].is_number(),
    });
    let detail = format!("fields present: {present}");
    if present["prompt_tokens"] == true && present["completion_tokens"] == true {
        pass("P15", detail)
    } else {
        fail("P15", detail)
    }
    .data(present)
}

pub(super) async fn p16(ep: &Endpoint) -> CheckResult {
    let prefix = "Reference text for the repeated-prefix test. ".repeat(400);
    let mut data = json!({});
    for (name, extra) in [
        ("reasoning_on", json!({})),
        (
            "reasoning_off",
            json!({"chat_template_kwargs":{"enable_thinking":false}}),
        ),
    ] {
        let body = ep.chat_body(json!([user(&format!("{prefix}\nSay OK."))]), extra);
        let mut times = Vec::new();
        for _ in 0..2 {
            match ep.chat_stream(&body).await {
                Ok(s) if (200..300).contains(&s.status) => {
                    times.push(s.first_chunk.map_or(-1, |d| d.as_millis() as i64));
                }
                Ok(s) => return fail("P16", format!("HTTP {}", s.status)),
                Err(e) => return error("P16", e),
            }
        }
        data[name] = json!({"cold_ms": times[0], "warm_ms": times[1]});
    }
    info("P16", format!("first chunk ms (cold, warm): {data}")).data(data)
}

pub(super) async fn p17(ep: &Endpoint) -> CheckResult {
    let body = ep.chat_body(
        json!([user("Hello.")]),
        json!({"tools":[weather_tool(), write_tool()],
            "tool_choice":{"type":"function","function":{"name":"get_weather"}}}),
    );
    match ep.chat(&body).await {
        Err(e) => error("P17", e),
        Ok(r) if !r.ok() => info(
            "P17",
            format!(
                "forced choice rejected: HTTP {} {}",
                r.status,
                truncate(&r.text, 200)
            ),
        ),
        Ok(r) => {
            let name = tool_calls(r.message())
                .first()
                .and_then(|c| c["function"]["name"].as_str().map(str::to_owned));
            if name.as_deref() == Some("get_weather") {
                pass("P17", "forced tool honoured")
            } else {
                fail("P17", format!("forced tool not called: {name:?}"))
            }
        }
    }
}

pub(super) async fn p18(ep: &Endpoint) -> CheckResult {
    let text = "Ľúbozvučný ďateľ žĺtne štvrtok";
    let note = tool(
        "save_note",
        "Save a note.",
        json!({"text":{"type":"string"}}),
        &["text"],
    );
    let body = ep.chat_body(
        json!([user(&format!(
            "Ulož poznámku s presne týmto textom: {text}. Použi nástroj."
        ))]),
        json!({"tools":[note],"tool_choice":"auto"}),
    );
    let reply = match ep.chat(&body).await {
        Ok(r) => r,
        Err(e) => return error("P18", e),
    };
    if !reply.ok() {
        return http_fail("P18", &reply);
    }
    let got = tool_calls(reply.message())
        .first()
        .and_then(super::checks::args)
        .and_then(|a| a["text"].as_str().map(str::to_owned));
    match got {
        Some(t) if t.contains(text) => pass("P18", "Slovak text verbatim in arguments"),
        Some(t) => fail("P18", format!("Slovak text altered: {}", truncate(&t, 120))),
        None => fail("P18", "no valid tool call"),
    }
}
