//! Read-only reports over a session: its turns, usage and a portable export
//! (port of `sessions/session-report.ts`). Pure queries over the store;
//! nothing here writes.

use super::SessionResult;
use super::descriptor::session_turn_kind;
use agentkit_store::TaskRecord;
use serde_json::{Map, Value};

pub const FALLBACK_CONTEXT_WINDOW_TOKENS: i64 = 32768;

/// The ONLY message metadata keys an export carries. An allow-list, because
/// the rest of that bag is host bookkeeping (`placeholder`,
/// `canonicalProvider*`) or caller-supplied, and provider continuation /
/// encrypted reasoning state must never leave the store.
const EXPORT_METADATA_KEYS: [&str; 5] = [
    "internal",
    "toolName",
    "steering",
    "inputId",
    "reasoningContent",
];

/// The session's turns (any status), oldest first.
pub async fn list_turn_tasks(
    store: &agentkit_store::SqliteStore,
    session_id: &str,
    kind: &str,
) -> SessionResult<Vec<TaskRecord>> {
    let mut tasks: Vec<TaskRecord> = store
        .list_tasks(Some(session_id.to_owned()))
        .await?
        .into_iter()
        .filter(|task| task.kind == kind)
        .collect();
    tasks.sort_by(|a, b| (a.enqueued_at, &a.task_id).cmp(&(b.enqueued_at, &b.task_id)));
    Ok(tasks)
}

pub fn summarize_turn(task: &TaskRecord) -> Value {
    let mut out = Map::new();
    out.insert("turnId".into(), task.task_id.clone().into());
    out.insert(
        "status".into(),
        serde_json::to_value(task.status).unwrap_or(Value::Null),
    );
    out.insert("attempts".into(), task.attempt_count.into());
    out.insert("createdAt".into(), task.enqueued_at.into());
    for (key, value) in [
        ("finishedAt", task.finished_at.map(Value::from)),
        (
            "model",
            task.payload["model"]
                .as_str()
                .filter(|text| !text.is_empty())
                .map(|text| text.into()),
        ),
        (
            "providerId",
            task.payload["providerId"]
                .as_str()
                .filter(|text| !text.is_empty())
                .map(|text| text.into()),
        ),
        (
            "error",
            task.error
                .as_deref()
                .filter(|text| !text.is_empty())
                .map(|text| text.into()),
        ),
    ] {
        if let Some(value) = value {
            out.insert(key.into(), value);
        }
    }
    Value::Object(out)
}

fn num(data: &Value, key: &str) -> f64 {
    data[key]
        .as_f64()
        .filter(|value| value.is_finite())
        .unwrap_or(0.0)
}

async fn turn_usage(
    store: &agentkit_store::SqliteStore,
    task: &TaskRecord,
) -> SessionResult<Value> {
    let mut requests = 0i64;
    let mut prompt_tokens = 0f64;
    let mut completion_tokens = 0f64;
    let mut total_tokens = 0f64;
    let mut last_prompt_tokens: Option<f64> = None;
    for event in store.events(&task.task_id, None, usize::MAX).await? {
        if event.event["type"] != "run.usage" {
            continue;
        }
        let data = &event.event["data"];
        // Only settled numbers: a non-final event is superseded by a later
        // one for the same call, so summing it would double count.
        if data["finalForCall"] != true {
            continue;
        }
        requests += 1;
        prompt_tokens += num(data, "promptTokens");
        completion_tokens += num(data, "completionTokens");
        total_tokens += num(data, "totalTokens");
        if data["promptTokens"].as_f64().is_some() {
            last_prompt_tokens = data["promptTokens"].as_f64();
        }
    }
    let mut out = Map::new();
    out.insert("turnId".into(), task.task_id.clone().into());
    out.insert("requests".into(), requests.into());
    out.insert("promptTokens".into(), prompt_tokens.into());
    out.insert("completionTokens".into(), completion_tokens.into());
    out.insert("totalTokens".into(), total_tokens.into());
    if let Some(value) = last_prompt_tokens {
        out.insert("lastPromptTokens".into(), value.into());
    }
    Ok(Value::Object(out))
}

/// Provider-reported token usage per turn, plus the context window it fills.
pub async fn session_usage(
    store: &agentkit_store::SqliteStore,
    session_id: &str,
    descriptor: &Value,
) -> SessionResult<Value> {
    let tasks = list_turn_tasks(store, session_id, &session_turn_kind(descriptor)).await?;
    let mut turns = vec![];
    for task in &tasks {
        turns.push(turn_usage(store, task).await?);
    }
    let mut totals = Map::new();
    let mut requests = 0i64;
    let mut prompt_tokens = 0f64;
    let mut completion_tokens = 0f64;
    let mut total_tokens = 0f64;
    let mut last_prompt_tokens: Option<f64> = None;
    for turn in &turns {
        requests += turn["requests"].as_i64().unwrap_or(0);
        prompt_tokens += turn["promptTokens"].as_f64().unwrap_or(0.0);
        completion_tokens += turn["completionTokens"].as_f64().unwrap_or(0.0);
        total_tokens += turn["totalTokens"].as_f64().unwrap_or(0.0);
        if let Some(value) = turn["lastPromptTokens"].as_f64() {
            last_prompt_tokens = Some(value);
        }
    }
    totals.insert("requests".into(), requests.into());
    totals.insert("promptTokens".into(), prompt_tokens.into());
    totals.insert("completionTokens".into(), completion_tokens.into());
    totals.insert("totalTokens".into(), total_tokens.into());
    // The newest turn's model wins: `setModel` changes it for future turns
    // only.
    let latest = tasks.last();
    let model_id = latest
        .and_then(|task| task.payload["model"].as_str())
        .or_else(|| descriptor["model"].as_str());
    let provider_id = latest
        .and_then(|task| task.payload["providerId"].as_str())
        .or_else(|| descriptor["providerId"].as_str());
    // The window chain: descriptor override, then the catalog's advertised
    // window, then the fallback (`resolveContextWindow` with no capabilities).
    let mut window_tokens = descriptor["context"]["windowTokens"]
        .as_i64()
        .filter(|value| *value > 0);
    let mut window_source = window_tokens.map(|_| "descriptor");
    if window_tokens.is_none() {
        let advertised = match provider_id {
            Some(provider) => store
                .get_setting("http.provider-catalog", provider)
                .await?
                .and_then(|catalog| {
                    catalog.as_array()?.iter().find(|model| {
                        model["modelId"].as_str() == model_id && model["modelId"].is_string()
                    })?["contextWindowTokens"]
                        .as_i64()
                        .filter(|value| *value > 0)
                }),
            None => None,
        };
        if advertised.is_some() {
            window_tokens = advertised;
            window_source = Some("model");
        }
    }
    let window_source = window_source.unwrap_or("fallback");
    let window_tokens = window_tokens.unwrap_or(FALLBACK_CONTEXT_WINDOW_TOKENS);
    let mut out = Map::new();
    out.insert("sessionId".into(), session_id.into());
    out.insert("turns".into(), turns.into());
    out.insert("totals".into(), Value::Object(totals));
    out.insert("contextWindowTokens".into(), window_tokens.into());
    out.insert("contextWindowSource".into(), window_source.into());
    if let Some(value) = last_prompt_tokens {
        out.insert("lastPromptTokens".into(), value.into());
    }
    Ok(Value::Object(out))
}

/// A JSON-serializable history: active-path messages, turns, inputs. No
/// secrets. (Persisted summaries have no Rust home yet — the compaction
/// milestone adds them, and this export will carry them then.)
pub async fn export_session(
    store: &agentkit_store::SqliteStore,
    session_id: &str,
    descriptor: &Value,
    chat_metadata: &Value,
    exported_at: &str,
) -> SessionResult<Value> {
    let records = store.messages(session_id, false).await?;
    let messages: Vec<Value> = records
        .iter()
        .map(|record| {
            let mut metadata = Map::new();
            for key in EXPORT_METADATA_KEYS {
                if let Some(value) = record.metadata.get(key) {
                    metadata.insert(key.into(), value.clone());
                }
            }
            let mut out = Map::new();
            out.insert("id".into(), record.id.clone().into());
            out.insert("role".into(), record.role.clone().into());
            out.insert("content".into(), record.content.clone());
            for (key, value) in [
                ("toolCalls", record.tool_calls.clone()),
                ("toolCallId", record.tool_call_id.clone().map(Value::from)),
                ("runId", record.run_id.clone().map(Value::from)),
            ] {
                if let Some(value) = value {
                    out.insert(key.into(), value);
                }
            }
            out.insert("metadata".into(), Value::Object(metadata));
            out.insert(
                "createdAt".into(),
                record
                    .created_at
                    .wire_text()
                    .map(|text| Value::from(text.into_owned()))
                    .unwrap_or(Value::Null),
            );
            Value::Object(out)
        })
        .collect();
    let mut out = Map::new();
    out.insert("format".into(), "agentkit.session.export".into());
    out.insert("version".into(), 1.into());
    out.insert("exportedAt".into(), exported_at.into());
    out.insert("sessionId".into(), session_id.into());
    out.insert("descriptor".into(), descriptor.clone());
    if let Some(execution) = agentkit_store::read_execution_state(chat_metadata) {
        out.insert("execution".into(), execution);
    }
    out.insert("messages".into(), messages.into());
    let turns: Vec<Value> = list_turn_tasks(store, session_id, &session_turn_kind(descriptor))
        .await?
        .iter()
        .map(summarize_turn)
        .collect();
    out.insert("turns".into(), turns.into());
    let inputs = store
        .list_inputs(agentkit_store::AgentInputQuery {
            session_id: session_id.to_owned(),
            ..Default::default()
        })
        .await?;
    out.insert(
        "inputs".into(),
        serde_json::to_value(&inputs).unwrap_or(Value::Array(vec![])),
    );
    Ok(Value::Object(out))
}
