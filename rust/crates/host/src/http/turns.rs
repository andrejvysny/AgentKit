use super::chats::string;
use super::*;
use agentkit_store::{MessageInput, SubmitTask, TaskRecord, TaskStatus};
use serde_json::json;

impl DesktopBackend {
    pub(super) async fn read_run(&self, id: &str) -> Result<TaskRecord, Problem> {
        self.host
            .store()
            .get_task(id)
            .await
            .map_err(store_error)?
            .filter(|t| t.payload["chatId"].as_str().is_some_and(|s| !s.is_empty()))
            .ok_or_else(|| missing("Run", id))
    }
    pub(super) async fn turns(
        &self,
        input: &OperationRequest,
    ) -> Result<Option<OperationResponse>, Problem> {
        let output = match input.operation.as_str() {
            "submitMessage" | "regenerateMessage" => self.submit_turn(input).await?,
            "getRun" => output(
                200,
                projections::run(self.read_run(&input.params["runId"]).await?),
            ),
            "cancelRun" => {
                let id = &input.params["runId"];
                self.read_run(id).await?;
                self.host.cancel(id).await.map_err(host_error)?;
                output(202, projections::run(self.read_run(id).await?))
            }
            "resumeRun" => {
                let id = &input.params["runId"];
                let task = self.read_run(id).await?;
                if task.status != TaskStatus::Interrupted {
                    return Err(Problem::new(
                        409,
                        "run_not_interrupted",
                        "Only an interrupted run can be resumed.",
                    ));
                }
                output(
                    202,
                    projections::run(self.host.resume(id).await.map_err(host_error)?),
                )
            }
            "listToolEvents" => self.tool_events(input).await?,
            _ => return Ok(None),
        };
        Ok(Some(output))
    }

    async fn submit_turn(&self, input: &OperationRequest) -> Result<OperationResponse, Problem> {
        if !self.host.supports("chat.turn") {
            return Err(Problem::host("executor_not_found", "chat.turn"));
        }
        let settings = self
            .host
            .store()
            .get_setting("http", "settings")
            .await
            .map_err(store_error)?
            .unwrap_or_else(|| json!({}));
        let provider_id = input
            .body
            .get("providerId")
            .and_then(Value::as_str)
            .or_else(|| settings.get("defaultProviderId").and_then(Value::as_str));
        let key = input.idempotency.as_ref().ok_or_else(|| {
            Problem::new(
                400,
                "idempotency_key_required",
                "Idempotency-Key is required.",
            )
        })?;
        let store = self.host.store();
        let chat_id = &input.params["chatId"];
        if store
            .get_chat(chat_id)
            .await
            .map_err(store_error)?
            .is_none()
        {
            return Err(missing("Chat", chat_id));
        }
        if let Some(existing) = store.get_task(&key.task_id).await.map_err(store_error)? {
            return replay(self.host.store(), existing, input).await;
        }
        let dynamic = self.host.resolves_provider_snapshots();
        if !dynamic
            && let Some(provider) = provider_id
            && self.pinned_provider_id.as_deref() != Some(provider)
        {
            return Err(Problem::new(
                400,
                "provider_not_bound",
                "Requested provider is not bound to this host executor.",
            ));
        }
        let regenerate = input.operation == "regenerateMessage";
        let (user_id, parent) = if regenerate {
            let id = &input.params["messageId"];
            let target = store
                .get_message(id)
                .await
                .map_err(store_error)?
                .filter(|m| m.chat_id == *chat_id)
                .ok_or_else(|| missing("Message", id))?;
            if target.role != "assistant"
                || target.parent_message_id.is_none()
                || target.metadata["internal"] == true
            {
                return Err(Problem::host(
                    "invalid_regenerate",
                    "Target must be an assistant answer with a user parent.",
                ));
            }
            let parent = target.parent_message_id.unwrap();
            let user = store
                .get_message(&parent)
                .await
                .map_err(store_error)?
                .filter(|m| m.role == "user")
                .ok_or_else(|| {
                    Problem::host(
                        "invalid_regenerate",
                        "Assistant parent must be a user message.",
                    )
                })?;
            (user.id, Some(parent))
        } else {
            let parent = string(&input.body, "parentMessageId");
            if let Some(id) = &parent
                && store
                    .get_message(id)
                    .await
                    .map_err(store_error)?
                    .is_none_or(|m| m.chat_id != *chat_id)
            {
                return Err(missing("Message", id));
            }
            (format!("{}_user", key.task_id), parent)
        };
        let assistant_id = format!("{}_assistant", key.task_id);
        let model = string(&input.body, "model")
            .or_else(|| string(&settings, "defaultModel"))
            .unwrap_or_else(|| self.default_model.clone());
        let provider_snapshot = if dynamic {
            let id = provider_id.ok_or_else(|| {
                Problem::new(
                    400,
                    "provider_required",
                    "An explicit provider is required for this host.",
                )
            })?;
            let id = agentkit_connections::ConnectionId::new(id)
                .map_err(super::providers::connection_error)?;
            let connection = self
                .connections
                .get(&id)
                .map_err(super::providers::connection_error)?;
            if !connection.config.enabled || !connection.config.models.iter().any(|m| m.id == model)
            {
                return Err(Problem::new(
                    400,
                    "unsupported_model",
                    "Selected connection/model is unavailable.",
                ));
            }
            Some(json!({"connection":connection,"model":model}))
        } else {
            None
        };
        let metadata = input.body.get("metadata").cloned().unwrap_or(json!({}));
        let task = SubmitTask {
            task_id: key.task_id.clone(),
            kind: "chat.turn".into(),
            scope_id: chat_id.clone(),
            payload: json!({"chatId":chat_id,"model":model,"providerId":provider_id,
                "userMessageId":user_id,"assistantMessageId":assistant_id,"httpRequest":input.body,
                "httpOperation":input.operation,"httpFingerprint":key.fingerprint,"httpTargetMessageId":input.params.get("messageId"),"providerSnapshot":provider_snapshot}),
            priority: 0,
            now: self.host.runtime().now_ms(),
            available_at: None,
            depends_on: vec![],
            parent_task_id: None,
            exclusive_scope: true,
        };
        let mut messages = Vec::new();
        if !regenerate {
            messages.push(MessageInput {
                id: Some(user_id.clone()),
                chat_id: chat_id.clone(),
                run_id: Some(key.task_id.clone()),
                content: input.body["content"].clone(),
                parent_message_id: parent,
                metadata: metadata.clone(),
                ..MessageInput::default()
            });
        }
        let mut placeholder_metadata = metadata;
        placeholder_metadata["placeholder"] = json!(true);
        messages.push(MessageInput {
            id: Some(assistant_id.clone()),
            chat_id: chat_id.clone(),
            run_id: Some(key.task_id.clone()),
            role: "assistant".into(),
            parent_message_id: Some(user_id.clone()),
            metadata: placeholder_metadata,
            ..MessageInput::default()
        });
        match store.submit_turn(task, messages).await {
            Ok(_) => Ok(output(
                201,
                json!({"chatId":chat_id,"runId":key.task_id,"userMessageId":user_id,"assistantMessageId":assistant_id}),
            )),
            Err(StoreError::InvalidInput(message))
                if message == "task replay identity mismatch" =>
            {
                replay(self.host.store(), self.read_run(&key.task_id).await?, input).await
            }
            Err(error) => Err(store_error(error)),
        }
    }

    async fn tool_events(&self, input: &OperationRequest) -> Result<OperationResponse, Problem> {
        let chat_id = &input.params["chatId"];
        let store = self.host.store();
        if store
            .get_chat(chat_id)
            .await
            .map_err(store_error)?
            .is_none()
        {
            return Err(missing("Chat", chat_id));
        }
        let mut tasks = store
            .list_tasks(Some(chat_id.clone()))
            .await
            .map_err(store_error)?;
        tasks.sort_by_key(|t| t.enqueued_at);
        let mut rows = Vec::new();
        for task in tasks {
            let mut after = None;
            loop {
                let events = store
                    .events_after_number(&task.task_id, after, 200)
                    .await
                    .map_err(store_error)?;
                let count = events.len();
                for row in events {
                    after = Some(row.seq);
                    if let Some(value) = tool_event(row.event, chat_id) {
                        rows.push(value);
                    }
                }
                if count < 200 {
                    break;
                }
            }
        }
        let skip = rows.len().saturating_sub(limit(input, 200));
        Ok(output(
            200,
            json!(rows.into_iter().skip(skip).collect::<Vec<_>>()),
        ))
    }
}

async fn replay(
    store: &agentkit_store::SqliteStore,
    task: TaskRecord,
    input: &OperationRequest,
) -> Result<OperationResponse, Problem> {
    let fields = if input.operation == "regenerateMessage" {
        &["model", "providerId"][..]
    } else {
        &[
            "content",
            "metadata",
            "model",
            "providerId",
            "parentMessageId",
        ][..]
    };
    let mut asked = serde_json::Map::new();
    let mut recorded = serde_json::Map::new();
    for &field in fields {
        if field == "parentMessageId" && input.body.get(field).is_none() {
            continue;
        }
        let default = if field == "metadata" {
            json!({})
        } else {
            Value::Null
        };
        asked.insert(
            field.into(),
            input.body.get(field).cloned().unwrap_or(default.clone()),
        );
        recorded.insert(
            field.into(),
            task.payload["httpRequest"]
                .get(field)
                .cloned()
                .unwrap_or(default),
        );
    }
    if input.operation == "submitMessage"
        && input.body.get("parentMessageId").is_some()
        && let Some(user_id) = task.payload["userMessageId"].as_str()
        && let Some(user) = store.get_message(user_id).await.map_err(store_error)?
    {
        recorded.insert("parentMessageId".into(), json!(user.parent_message_id));
    }
    if agentkit_contracts::canonical_json(&Value::Object(asked))
        != agentkit_contracts::canonical_json(&Value::Object(recorded))
        || task.payload["httpOperation"] != input.operation
        || task.payload["chatId"].as_str() != input.params.get("chatId").map(String::as_str)
        || (input.operation == "regenerateMessage"
            && task.payload["httpTargetMessageId"].as_str()
                != input.params.get("messageId").map(String::as_str))
    {
        return Err(Problem::new(
            422,
            "idempotency_key_mismatch",
            "This `Idempotency-Key` was already used for a different request. A retry must send the same body; a new request needs a new key.",
        ));
    }
    Ok(output(
        200,
        json!({"chatId":task.payload["chatId"],"runId":task.task_id,"userMessageId":task.payload["userMessageId"],"assistantMessageId":task.payload["assistantMessageId"]}),
    ))
}

fn tool_event(event: Value, chat_id: &str) -> Option<Value> {
    let status = match event["type"].as_str()? {
        "run.tool.requested" => "requested",
        "run.tool.running" => "running",
        "run.tool.succeeded" => "succeeded",
        "run.tool.failed" => "failed",
        _ => return None,
    };
    let data = &event["data"];
    let tool_call_id = data["toolCallId"].as_str()?;
    let tool_name = data["toolName"].as_str()?;
    let mut value = json!({"id":event["eventId"],"runId":event["runId"],"chatId":chat_id,
        "toolCallId":tool_call_id,"toolName":tool_name,"status":status,"createdAt":event["timestamp"]});
    for key in [
        "argumentsJson",
        "resultJson",
        "modelResultJson",
        "summary",
        "errorCode",
        "errorMessage",
    ] {
        if let Some(text) = data.get(key).and_then(Value::as_str) {
            value[key] = json!(text);
        }
    }
    for key in ["sources", "warnings"] {
        if let Some(array) = data.get(key).and_then(Value::as_array) {
            value[key] = json!(array);
        }
    }
    if let Some(truncated) = data.get("truncated").and_then(Value::as_bool) {
        value["truncated"] = json!(truncated);
    }
    Some(value)
}
