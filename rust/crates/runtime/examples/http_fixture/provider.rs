use agentkit_core::{ChatProvider, ChatRequest, ProviderEvent, ProviderStream};
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::post,
};
use serde_json::Value;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
pub struct Scripted {
    scripts: Mutex<Option<Vec<Value>>>,
}
impl Scripted {
    fn turn(&self) -> Value {
        let mut scripts = self.scripts.lock().unwrap();
        if let Some(turns) = scripts.as_mut() {
            return if turns.is_empty() {
                serde_json::json!({"steps":[]})
            } else {
                turns.remove(0)
            };
        }
        serde_json::json!({"steps":[{"kind":"text","content":"Hello, "},{"kind":"text","content":"client"},{"kind":"text","content":"."}]})
    }
}
impl ChatProvider for Scripted {
    fn id(&self) -> &str {
        "p1"
    }
    fn stream_chat(&self, input: ChatRequest, cancel: CancellationToken) -> ProviderStream {
        let parked = input.messages.last().and_then(|m| m.content.as_str()) == Some("park");
        let turn = self.turn();
        let mut events = Vec::new();
        let mut content = String::new();
        for step in turn["steps"].as_array().unwrap() {
            let delta = step["content"].as_str().unwrap();
            content.push_str(delta);
            events.push(ProviderEvent::Delta(delta.into()));
        }
        events.push(ProviderEvent::Completed {
            content,
            tool_calls: vec![],
            finish_reason: turn["finishReason"].as_str().unwrap_or("stop").into(),
        });
        Box::pin(futures::stream::unfold(
            (events.into_iter(), 0usize, cancel),
            move |(mut events, index, cancel)| async move {
                if cancel.is_cancelled() {
                    return None;
                }
                tokio::time::sleep(std::time::Duration::from_millis(8)).await;
                let event = if parked {
                    if index == 0 {
                        ProviderEvent::Delta("waiting".into())
                    } else {
                        cancel.cancelled().await;
                        return None;
                    }
                } else {
                    events.next()?
                };
                Some((Ok(event), (events, index + 1, cancel)))
            },
        ))
    }
}

#[derive(Clone)]
struct Control {
    provider: Arc<Scripted>,
    token: String,
    store: agentkit_store::SqliteStore,
    native: Arc<agentkit_connections::NativeConnections>,
    secrets: agentkit_connections::SharedSecretStore,
    connections: agentkit_connections::AppConnections,
}
pub fn control_router(
    provider: Arc<Scripted>,
    token: String,
    store: agentkit_store::SqliteStore,
    native: Arc<agentkit_connections::NativeConnections>,
    secrets: agentkit_connections::SharedSecretStore,
    connections: agentkit_connections::AppConnections,
) -> Router {
    Router::new()
        .route("/script", post(set_script))
        .route("/proposal", post(stage_proposal))
        .route("/secret", post(read_secret))
        .route("/secrets", post(secret_snapshot))
        .route("/tasks", post(tasks))
        .with_state(Control {
            provider,
            token,
            store,
            native,
            secrets,
            connections,
        })
}
async fn set_script(
    State(state): State<Control>,
    headers: HeaderMap,
    Json(value): Json<Value>,
) -> StatusCode {
    if headers.get("authorization").and_then(|h| h.to_str().ok())
        != Some(format!("Bearer {}", state.token).as_str())
    {
        return StatusCode::UNAUTHORIZED;
    }
    let Some(turns) = value.as_array() else {
        return StatusCode::BAD_REQUEST;
    };
    for turn in turns {
        let Some(steps) = turn["steps"].as_array() else {
            return StatusCode::BAD_REQUEST;
        };
        if steps
            .iter()
            .any(|s| s["kind"] != "text" || !s["content"].is_string())
        {
            return StatusCode::BAD_REQUEST;
        }
    }
    *state.provider.scripts.lock().unwrap() = Some(turns.clone());
    StatusCode::NO_CONTENT
}

async fn stage_proposal(
    State(state): State<Control>,
    headers: HeaderMap,
    Json(value): Json<Value>,
) -> Result<Json<Value>, StatusCode> {
    if headers.get("authorization").and_then(|h| h.to_str().ok())
        != Some(format!("Bearer {}", state.token).as_str())
    {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let chat = value["chatId"].as_str().ok_or(StatusCode::BAD_REQUEST)?;
    if state
        .store
        .get_chat(chat)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .is_none()
    {
        return Err(StatusCode::NOT_FOUND);
    }
    let now = chrono::Utc::now().timestamp_millis();
    let record = serde_json::from_value(serde_json::json!({
        "id":value.get("id").cloned().unwrap_or_else(||serde_json::json!(uuid::Uuid::new_v4().to_string())),
        "chatId":chat,"runId":value["runId"],"scopeKey":value["scopeKey"],
        "actionId":value["actionId"],"toolName":value["toolName"],"kind":value["kind"],
        "risk":value["risk"],"status":"pending","envelope":value.get("envelope").cloned().unwrap_or(serde_json::json!({})),
        "operations":value["operations"],"warnings":value.get("warnings").cloned().unwrap_or(serde_json::json!([])),
        "truncated":value.get("truncated").cloned().unwrap_or(serde_json::json!(false)),
        "revisionAtCreate":value["revisionAtCreate"],"operationId":null,"decision":null,"reason":null,
        "createdAt":now,"decidedAt":null,"claimedAt":null,"appliedAt":null
    })).map_err(|_|StatusCode::BAD_REQUEST)?;
    let record = state
        .store
        .create_proposal(record)
        .await
        .map_err(|_| StatusCode::CONFLICT)?;
    let mut result = serde_json::to_value(record).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    result["createdAt"] = serde_json::json!(
        chrono::DateTime::from_timestamp_millis(now)
            .unwrap()
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    );
    Ok(Json(result))
}

async fn read_secret(
    State(state): State<Control>,
    headers: HeaderMap,
    Json(value): Json<Value>,
) -> Result<Json<Value>, StatusCode> {
    use agentkit_connections::{ConnectionError, ConnectionId};
    if headers.get("authorization").and_then(|h| h.to_str().ok())
        != Some(format!("Bearer {}", state.token).as_str())
    {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let id = value["ref"]
        .as_str()
        .and_then(|s| s.strip_prefix("provider/"))
        .and_then(|s| s.strip_suffix("/api-key"))
        .ok_or(StatusCode::BAD_REQUEST)?;
    let id = ConnectionId::new(id).map_err(|_| StatusCode::BAD_REQUEST)?;
    let key = state
        .native
        .credential_key(&id)
        .map_err(|_| StatusCode::NOT_FOUND)?;
    match state.secrets.get(&key).await {
        Ok(secret) => Ok(Json(serde_json::json!(secret.expose()))),
        Err(ConnectionError::SecretMissing) => Ok(Json(Value::Null)),
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

async fn tasks(
    State(state): State<Control>,
    headers: HeaderMap,
    Json(value): Json<Value>,
) -> Result<Json<Value>, StatusCode> {
    if headers.get("authorization").and_then(|h| h.to_str().ok())
        != Some(format!("Bearer {}", state.token).as_str())
    {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let store = &state.store;
    let id = value["taskId"].as_str().unwrap_or("");
    let result = match value["operation"].as_str() {
        Some("getTask") => encoded(store.get_task(id).await.map_err(control_error)?)?,
        Some("listByScope") => encoded(
            store
                .list_tasks(value["scopeId"].as_str().map(str::to_owned))
                .await
                .map_err(control_error)?,
        )?,
        Some("listEvents") => task_events(store, id, &value["options"]).await?,
        Some("acquireLease") => task_lease(store, &value["input"]).await?,
        Some("appendEvents") => task_append(store, &value).await?,
        Some("nextSeq") => encoded(store.next_sequence(id).await.map_err(control_error)?)?,
        Some("seedNumericRun") => seed_numeric_run(store, &value).await?,
        _ => return Err(StatusCode::BAD_REQUEST),
    };
    Ok(Json(result))
}
fn control_error(_: impl std::fmt::Display) -> StatusCode {
    StatusCode::INTERNAL_SERVER_ERROR
}
fn encoded(value: impl serde::Serialize) -> Result<Value, StatusCode> {
    serde_json::to_value(value).map_err(control_error)
}
async fn task_events(
    store: &agentkit_store::SqliteStore,
    id: &str,
    options: &Value,
) -> Result<Value, StatusCode> {
    encoded(
        store
            .events_after_number(
                id,
                agentkit_contracts::javascript_number(&options["afterSeq"]),
                options["limit"].as_u64().unwrap_or(10000) as usize,
            )
            .await
            .map_err(control_error)?
            .into_iter()
            .map(|record| record.event)
            .collect::<Vec<_>>(),
    )
}
async fn task_lease(
    store: &agentkit_store::SqliteStore,
    input: &Value,
) -> Result<Value, StatusCode> {
    let id = input["taskId"].as_str().ok_or(StatusCode::BAD_REQUEST)?;
    let attempt = input["attemptId"].as_str().ok_or(StatusCode::BAD_REQUEST)?;
    let owner = input["ownerId"].as_str().ok_or(StatusCode::BAD_REQUEST)?;
    let ttl = input["ttlMs"].as_i64().ok_or(StatusCode::BAD_REQUEST)?;
    encoded(
        store
            .acquire_lease(
                id,
                attempt,
                owner,
                chrono::Utc::now().timestamp_millis(),
                ttl,
            )
            .await
            .map_err(|_| StatusCode::CONFLICT)?,
    )
}
async fn task_append(
    store: &agentkit_store::SqliteStore,
    value: &Value,
) -> Result<Value, StatusCode> {
    let proof =
        serde_json::from_value(value["proof"].clone()).map_err(|_| StatusCode::BAD_REQUEST)?;
    let events = value["events"]
        .as_array()
        .ok_or(StatusCode::BAD_REQUEST)?
        .clone();
    encoded(
        store
            .append_events(proof, events, chrono::Utc::now().timestamp_millis())
            .await
            .map_err(|_| StatusCode::CONFLICT)?,
    )
}

async fn secret_snapshot(
    State(state): State<Control>,
    headers: HeaderMap,
) -> Result<Json<Value>, StatusCode> {
    if headers.get("authorization").and_then(|h| h.to_str().ok())
        != Some(format!("Bearer {}", state.token).as_str())
    {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let mut result = serde_json::Map::new();
    for connection in state
        .connections
        .list()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    {
        let key = state
            .native
            .credential_key_at(&connection.id, connection.generation)
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        match state.secrets.get(&key).await {
            Ok(secret) => {
                result.insert(
                    format!("provider/{}/api-key", connection.id.as_str()),
                    serde_json::json!(secret.expose()),
                );
            }
            Err(agentkit_connections::ConnectionError::SecretMissing) => {}
            Err(_) => return Err(StatusCode::INTERNAL_SERVER_ERROR),
        }
    }
    Ok(Json(Value::Object(result)))
}

async fn seed_numeric_run(
    store: &agentkit_store::SqliteStore,
    value: &Value,
) -> Result<Value, StatusCode> {
    let id = value["taskId"].as_str().ok_or(StatusCode::BAD_REQUEST)?;
    let events = value["events"]
        .as_array()
        .ok_or(StatusCode::BAD_REQUEST)?
        .clone();
    let now = chrono::Utc::now().timestamp_millis();
    store
        .submit(agentkit_store::SubmitTask {
            task_id: id.into(),
            kind: "fixture.sequence".into(),
            scope_id: "chat-client".into(),
            payload: serde_json::json!({"chatId":"chat-client"}),
            priority: 0,
            now,
            available_at: None,
            depends_on: vec![],
            parent_task_id: None,
            exclusive_scope: false,
        })
        .await
        .map_err(control_error)?;
    store.cancel_task(id, now).await.map_err(control_error)?;
    let lease = store
        .acquire_lease(id, "numeric-attempt", "numeric-fixture", now, 60_000)
        .await
        .map_err(control_error)?;
    store
        .append_events(lease.fence(), events, now)
        .await
        .map_err(control_error)?;
    Ok(serde_json::json!({"runId":id}))
}
