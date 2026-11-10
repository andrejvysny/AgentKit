use super::chats::string;
use super::*;
use agentkit_chat::ChatHostExt;
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
        if !self.host.supports(agentkit_sessions::CHAT_TURN_TASK_KIND) {
            return Err(Problem::host(
                "executor_not_found",
                agentkit_sessions::CHAT_TURN_TASK_KIND,
            ));
        }
        let store = self.host.store();
        let chat_id = &input.params["chatId"];
        let chat = store
            .get_chat(chat_id)
            .await
            .map_err(store_error)?
            .ok_or_else(|| missing("Chat", chat_id))?;
        let key = input.idempotency.as_ref().ok_or_else(|| {
            Problem::new(
                400,
                "idempotency_key_required",
                "Idempotency-Key is required.",
            )
        })?;
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
        let descriptor = agentkit_sessions::read_session_descriptor(&chat.metadata);
        if descriptor.is_none()
            && (self.session_profile.is_some()
                || chat
                    .metadata
                    .get(agentkit_store::SESSION_METADATA_KEY)
                    .is_some())
        {
            return Err(Problem::new(
                400,
                "session_required",
                "A valid host-owned session descriptor is required.",
            ));
        }
        let session_config = if descriptor.is_some() {
            let service = agentkit_sessions::AgentSessionService::new(
                agentkit_sessions::AgentSessionServiceDeps {
                    store: store.clone(),
                    runtime: self.host.runtime().clone(),
                    plan_approval: None,
                    wake: None,
                },
            )
            .with_profiles(self.profiles.clone());
            let service = match &self.content_authority {
                Some(authority) => service.with_content_authority(authority.clone()),
                None => service,
            };
            service
                .snapshot_turn_configuration(chat_id)
                .await
                .map_err(|e| Problem::new(400, "invalid_request", e.to_string()))?
        } else {
            json!({})
        };
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
            .or_else(|| session_config.get("providerId").and_then(Value::as_str))
            .or_else(|| settings.get("defaultProviderId").and_then(Value::as_str));
        let dynamic = self.host.resolves_provider_snapshots();
        if !dynamic
            && self.provider_factory.is_none()
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
            .or_else(|| string(&session_config, "model"))
            .or_else(|| string(&settings, "defaultModel"))
            .unwrap_or_else(|| self.default_model.clone());
        let provider_snapshot = if let Some(factory) = &self.provider_factory {
            match provider_id {
                Some(provider_id) => {
                    let cancel = agentkit_core::CancellationToken::new();
                    let _cancel_on_drop = cancel.clone().drop_guard();
                    tokio::time::timeout(
                        std::time::Duration::from_secs(10),
                        factory.snapshot(provider_id, &model, cancel),
                    )
                    .await
                    .map_err(|_| {
                        Problem::new(
                            504,
                            "provider_snapshot_timeout",
                            "Provider snapshot authority did not answer.",
                        )
                    })?
                    .map_err(provider_snapshot_error)?
                }
                None => None,
            }
        } else if dynamic {
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
        let selection = input
            .body
            .get("contentSelection")
            .map(|selection| {
                let mut selected: agentkit_sessions::RunContentSelection =
                    serde_json::from_value(selection.clone()).map_err(|e| {
                        Problem::new(
                            400,
                            "invalid_request",
                            format!("Invalid content selection: {e}"),
                        )
                    })?;
                if let Some(base) = session_config.get("contentSelection") {
                    let base: agentkit_sessions::RunContentSelection =
                        serde_json::from_value(base.clone()).map_err(|e| {
                            Problem::host("invalid_content_selection", e.to_string())
                        })?;
                    for skill in base.skills {
                        if !selected.skills.iter().any(|s| s.id == skill.id) {
                            selected.skills.push(skill);
                        }
                    }
                }
                serde_json::to_value(selected)
                    .map_err(|e| Problem::host("invalid_content_selection", e.to_string()))
            })
            .transpose()?;
        let content_snapshot = match &selection {
            Some(selection) => Some(
                agentkit_sessions::freeze_run_content(
                    store,
                    selection.clone(),
                    chat_id,
                    self.content_authority.as_ref(),
                )
                .await
                .map_err(session_error)?,
            ),
            None => None,
        };
        let metadata = input.body.get("metadata").cloned().unwrap_or(json!({}));
        let reasoning_effort: Option<agentkit_core::ReasoningEffort> = input
            .body
            .get("reasoningEffort")
            .filter(|v| !v.is_null())
            .map(|v| serde_json::from_value(v.clone()))
            .transpose()
            .map_err(|_| Problem::new(400, "invalid_request", "Invalid reasoningEffort."))?;
        let mut task = SubmitTask {
            task_id: key.task_id.clone(),
            kind: agentkit_sessions::CHAT_TURN_TASK_KIND.into(),
            scope_id: chat_id.clone(),
            payload: json!({"chatId":chat_id,"model":model,"providerId":provider_id,
                "userMessageId":user_id,"assistantMessageId":assistant_id,"httpRequest":input.body,
                "httpOperation":input.operation,"httpFingerprint":key.fingerprint,"httpTargetMessageId":input.params.get("messageId"),"providerSnapshot":provider_snapshot,"contentSnapshot":content_snapshot,"contentSelection":selection,"reasoningEffort":input.body.get("reasoningEffort")}),
            priority: 0,
            now: self.host.runtime().now_ms(),
            available_at: None,
            depends_on: vec![],
            parent_task_id: None,
            exclusive_scope: true,
        };
        let mut payload = session_config.as_object().cloned().unwrap_or_default();
        payload.extend(task.payload.as_object().expect("turn payload").clone());
        if content_snapshot.is_none() {
            for key in ["contentSnapshot", "contentSelection"] {
                if let Some(value) = session_config.get(key) {
                    payload.insert(key.into(), value.clone());
                }
            }
        }
        if let Some(effort) = reasoning_effort {
            payload.insert(
                "reasoningEffort".into(),
                serde_json::to_value(effort).expect("effort"),
            );
        } else if let Some(value) = session_config.get("reasoningEffort") {
            payload.insert("reasoningEffort".into(), value.clone());
        }
        task.payload = Value::Object(payload);
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
            Err(StoreError::TaskInput {
                reason: agentkit_engine::TaskInputReason::ReplayIdentityMismatch,
                ..
            }) => replay(self.host.store(), self.read_run(&key.task_id).await?, input).await,
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
