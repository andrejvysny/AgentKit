//! The probe against a local mock that replays recorded server behaviours.
use agentkit_providers::probe::{Endpoint, Report, Secret, Status, run_checks};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[derive(Clone, Default)]
struct Behaviour {
    /// Tool calls come back as `<tool_call>` text, not `tool_calls`.
    no_tool_parser: bool,
    corrupt_text: bool,
    loose_types: bool,
    reject_extra_params: bool,
    no_reasoning: bool,
    /// Every Nth required-tool request returns no call.
    drop_every: usize,
    /// Error bodies echo the Authorization header.
    echo_key: bool,
    /// Echoed into forced-call function names, argument text and error bodies.
    leak: Option<String>,
}

fn call(id: &str, name: &str, args: Value) -> Value {
    json!({"id":id,"type":"function","function":{"name":name,"arguments":args.to_string()}})
}

fn reply(b: &Behaviour, body: &Value, n: usize, auth: &str) -> (u16, Value) {
    let messages = body["messages"].as_array().cloned().unwrap_or_default();
    let systems = messages.iter().filter(|m| m["role"] == "system").count();
    if let Some(leak) = &b.leak
        && systems > 1
    {
        return (
            400,
            json!({"error":{"message":format!("bad request, key {leak} / {}", json!(leak))}}),
        );
    }
    if b.echo_key && systems > 1 {
        return (
            400,
            json!({"error":{"message":format!("bad request, header {auth}")}}),
        );
    }
    if systems > 1 {
        return (
            400,
            json!({"error":{"message":"Only one system message allowed"}}),
        );
    }
    if b.reject_extra_params && body.get("top_k").is_some() {
        return (400, json!({"error":{"message":"unknown field top_k"}}));
    }
    let usage = json!({"prompt_tokens":10,"completion_tokens":5,
        "prompt_tokens_details":{"cached_tokens":0},"completion_tokens_details":{"reasoning_tokens":2}});
    let mut message = json!({"role":"assistant","content":"391"});
    let last = messages.last().cloned().unwrap_or_default();
    let prompt = last["content"].as_str().unwrap_or_default().to_string();
    let tools = body["tools"].as_array().cloned().unwrap_or_default();
    let forced = body["tool_choice"]["function"]["name"]
        .as_str()
        .map(str::to_owned);
    let first = forced.clone().or_else(|| {
        tools
            .first()
            .and_then(|t| t["function"]["name"].as_str().map(str::to_owned))
    });
    if last["role"] != "tool"
        && let Some(name) = first
    {
        let calls = match name.as_str() {
            "get_weather" if prompt.contains("Paris and in Rome") => vec![
                call("call_a", &name, json!({"city":"Paris"})),
                call("call_b", &name, json!({"city":"Rome"})),
            ],
            "get_weather" if forced.is_some() && b.leak.is_some() => {
                let leaked = format!("{name}_{}", b.leak.clone().unwrap_or_default());
                vec![call("call_a", &leaked, json!({"city":"Paris"}))]
            }
            "get_weather" => vec![call("call_a", &name, json!({"city":"Paris"}))],
            "write_file" => {
                let text = prompt
                    .split("<<<\n")
                    .nth(1)
                    .and_then(|r| r.split("\n>>>").next())
                    .unwrap_or("");
                let text = if b.corrupt_text {
                    text.replace('"', "'")
                } else {
                    text.to_string()
                };
                vec![call(
                    "call_a",
                    &name,
                    json!({"path":"notes.txt","content":text}),
                )]
            }
            "set_options" => {
                let count = if b.loose_types { json!("3") } else { json!(3) };
                vec![call(
                    "call_a",
                    &name,
                    json!({"count":count,"verbose":true,"tags":["a","b"],"range":{"from":1,"to":5}}),
                )]
            }
            _ => {
                let text = prompt
                    .split("textom: ")
                    .nth(1)
                    .and_then(|r| r.split(". Použi").next())
                    .unwrap_or("");
                let text = match &b.leak {
                    Some(leak) => format!("{text} {leak}"),
                    None => text.to_string(),
                };
                vec![call("call_a", &name, json!({"text":text}))]
            }
        };
        let dropped =
            b.drop_every > 0 && body["tool_choice"] == "required" && n.is_multiple_of(b.drop_every);
        if b.no_tool_parser {
            message["content"] = format!("<tool_call>{}</tool_call>", calls[0]["function"]).into();
        } else if dropped {
            message["content"] = "I will think about it.".into();
        } else {
            message["tool_calls"] = calls.into();
            message["content"] = Value::Null;
        }
    }
    let thinking_off = body["chat_template_kwargs"]["enable_thinking"] == false;
    if !b.no_reasoning && !thinking_off {
        message["reasoning_content"] = "17*23 = 391".into();
    }
    (
        200,
        json!({"choices":[{"index":0,"message":message,"finish_reason":"stop"}],"usage":usage}),
    )
}

fn sse(message: &Value) -> String {
    let mut out = String::new();
    let mut push = |delta: Value| {
        out.push_str(&format!(
            "data: {}\n\n",
            json!({"choices":[{"index":0,"delta":delta}]})
        ))
    };
    if let Some(r) = message["reasoning_content"].as_str() {
        push(json!({"reasoning_content": r}));
    }
    for (i, c) in message["tool_calls"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .enumerate()
    {
        let args = c["function"]["arguments"].as_str().unwrap_or("");
        let (a, b) = args.split_at(args.len() / 2);
        push(
            json!({"tool_calls":[{"index":i,"id":c["id"],"type":"function","function":{"name":c["function"]["name"],"arguments":a}}]}),
        );
        push(json!({"tool_calls":[{"index":i,"function":{"arguments":b}}]}));
    }
    if let Some(t) = message["content"].as_str() {
        push(json!({"content": t}));
    }
    out.push_str("data: [DONE]\n\n");
    out
}

async fn serve(b: Behaviour) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let counter = Arc::new(AtomicUsize::new(0));
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let (b, counter) = (b.clone(), counter.clone());
            tokio::spawn(async move {
                let mut data = Vec::new();
                let (head, body) = loop {
                    let mut chunk = [0u8; 8192];
                    let read = socket.read(&mut chunk).await.unwrap_or(0);
                    if read == 0 {
                        return;
                    }
                    data.extend_from_slice(&chunk[..read]);
                    let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") else {
                        continue;
                    };
                    let head = String::from_utf8_lossy(&data[..end]).to_string();
                    let length = head
                        .lines()
                        .find_map(|l| {
                            l.to_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    if data.len() >= end + 4 + length {
                        break (head, data[end + 4..end + 4 + length].to_vec());
                    }
                };
                let line = head.lines().next().unwrap_or_default().to_string();
                let auth = head
                    .lines()
                    .find(|l| l.to_lowercase().starts_with("authorization:"))
                    .unwrap_or_default()
                    .to_string();
                let (status, content_type, payload) = if line.starts_with("GET /v1/models") {
                    (
                        200,
                        "application/json",
                        json!({"data":[{"id":"m","max_model_len":131072}]}).to_string(),
                    )
                } else if line.starts_with("GET /v1/model/info") {
                    (200, "application/json", json!({"data":[]}).to_string())
                } else {
                    let body: Value = serde_json::from_slice(&body).unwrap_or_default();
                    let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
                    let (status, value) = reply(&b, &body, n, &auth);
                    if body["stream"] == true && status == 200 {
                        (
                            200,
                            "text/event-stream",
                            sse(&value["choices"][0]["message"]),
                        )
                    } else {
                        (status, "application/json", value.to_string())
                    }
                };
                let response = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}",
                    payload.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    url
}

const KEY: &str = "sk-probe-secret-123";
async fn probe(b: Behaviour, only: &[&str]) -> Report {
    let url = serve(b).await;
    let only: Vec<String> = only.iter().map(|s| s.to_string()).collect();
    run_checks(&Endpoint::new(&url, "m", Secret::new(KEY)), &only, 10).await
}
fn status(report: &Report, id: &str) -> Status {
    report
        .get(id)
        .unwrap_or_else(|| panic!("{id} missing"))
        .status
}

#[tokio::test]
async fn compliant_server_passes_every_check() {
    let report = probe(Behaviour::default(), &[]).await;
    assert_eq!(report.results.len(), 18);
    for result in &report.results {
        assert!(
            matches!(result.status, Status::Pass | Status::Info),
            "{} {:?} {}",
            result.id,
            result.status,
            result.detail
        );
    }
}

#[tokio::test]
async fn each_broken_behaviour_fails_its_check() {
    let cases: [(Behaviour, &[&str]); 6] = [
        (
            Behaviour {
                no_tool_parser: true,
                ..Default::default()
            },
            &["P1", "P3", "P4", "P9", "P10", "P17", "P18"],
        ),
        (
            Behaviour {
                corrupt_text: true,
                ..Default::default()
            },
            &["P9"],
        ),
        (
            Behaviour {
                loose_types: true,
                ..Default::default()
            },
            &["P10"],
        ),
        (
            Behaviour {
                reject_extra_params: true,
                ..Default::default()
            },
            &["P8"],
        ),
        (
            Behaviour {
                no_reasoning: true,
                ..Default::default()
            },
            &["P5"],
        ),
        (
            Behaviour {
                drop_every: 2,
                ..Default::default()
            },
            &["P11"],
        ),
    ];
    for (behaviour, expected) in cases {
        let report = probe(behaviour, expected).await;
        for id in expected {
            assert_eq!(
                status(&report, id),
                Status::Fail,
                "{id}: {:?}",
                report.get(id)
            );
        }
    }
}

#[tokio::test]
async fn key_never_reaches_the_report() {
    let report = probe(
        Behaviour {
            echo_key: true,
            ..Default::default()
        },
        &["P12", "P1"],
    )
    .await;
    let p12 = report.get("P12").unwrap();
    assert!(p12.detail.contains("rejected"), "{}", p12.detail);
    assert!(p12.detail.contains("[redacted]"), "{}", p12.detail);
    let all = format!(
        "{}{}{:?}",
        report.to_json(),
        report.to_markdown(),
        Secret::new(KEY)
    );
    assert!(!all.contains(KEY));
}

#[tokio::test]
async fn unreachable_endpoint_is_an_error_not_a_pass() {
    let report = run_checks(
        &Endpoint::new("http://127.0.0.1:1/v1", "m", Secret::new(KEY)),
        &["P1".into()],
        1,
    )
    .await;
    assert_eq!(status(&report, "P1"), Status::Error);
}

#[tokio::test]
async fn key_in_parsed_values_and_escaped_forms_never_reaches_the_report() {
    for key in [KEY, "sk-\"q\\x/+ y"] {
        let url = serve(Behaviour {
            leak: Some(key.to_string()),
            ..Default::default()
        })
        .await;
        let only: Vec<String> = ["P12", "P17", "P18"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let report = run_checks(&Endpoint::new(&url, "m", Secret::new(key)), &only, 1).await;
        let escaped = serde_json::to_string(key).unwrap();
        let escaped = &escaped[1..escaped.len() - 1];
        let all = format!("{}{}{:?}", report.to_json(), report.to_markdown(), report);
        assert!(!all.contains(key), "raw key leaked: {all}");
        assert!(!all.contains(escaped), "escaped key leaked: {all}");
        assert!(all.contains("[redacted]"), "{all}");
    }
}

#[test]
fn secret_scrubs_value_trees_and_encoded_forms() {
    let secret = Secret::new("sk-a b+/");
    let value = json!({"k sk-a b+/": ["x sk-a%20b%2B%2F y", {"n": "sk-a b+/"}], "n": 3});
    let out = secret.scrub_value(&value).to_string();
    assert!(!out.contains("sk-a"), "{out}");
    assert!(!secret.scrub("c2stYSBiKy8=").contains("c2stYSBiKy8="));
}
