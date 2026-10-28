use crate::*;
use agentkit_core::{
    ChatMessage, ChatProvider, ContinuationScope, CoreError, RunEventSink, ToolCall,
};
use agentkit_store::{MessageInput, MessageRecord, order_messages_for_provider};
use async_trait::async_trait;
use std::collections::HashSet;

pub(super) struct ContinuationSession {
    pub state: Option<Value>,
    pub scope: ContinuationScope,
}
pub(super) async fn prepare_continuation(
    context: &ExecutionContext,
    records: &[MessageRecord],
    provider: &dyn ChatProvider,
    chat_id: &str,
    model: &str,
    anchor: &str,
) -> HostResult<Option<ContinuationSession>> {
    let mut runs = HashSet::new();
    for message in records {
        if let Some(run) = &message.run_id
            && runs.insert(run)
        {
            context.store.get_continuation(run).await?;
        }
    }
    let current = context
        .store
        .get_continuation(&context.task.task_id)
        .await?;
    let mut candidates = context
        .store
        .continuations_by_anchors(
            chat_id,
            records
                .iter()
                .filter(|message| message.id != anchor)
                .map(|message| message.id.clone())
                .collect(),
        )
        .await?;
    let previous = current
        .or_else(|| candidates.pop())
        .map(|record| record.state);
    let identity = provider.connection_identity();
    if identity
        .as_ref()
        .is_none_or(|identity| identity.protocol != "responses")
    {
        if previous.is_some() {
            return Err(HostError::Execution(
                "provider_continuation_scope_mismatch".into(),
            ));
        }
        return Ok(None);
    }
    let identity = identity.expect("checked");
    if identity.provider_id != provider.id() {
        return Err(HostError::Execution("invalid_provider_connection".into()));
    }
    if !records.iter().any(|message| message.id == anchor) {
        return Err(HostError::Execution(
            "provider_continuation_branch_mismatch".into(),
        ));
    }
    let branch = records
        .iter()
        .rev()
        .find(|message| message.branch_index > 0)
        .or_else(|| records.first())
        .ok_or_else(|| HostError::Execution("provider_continuation_branch_mismatch".into()))?;
    let scope = json!({"providerId":identity.provider_id,"protocol":identity.protocol,"connectionId":identity.connection_id,"generation":identity.generation,
        "chatId":chat_id,"branchId":branch.id,"model":model});
    if previous
        .as_ref()
        .is_some_and(|state| state["scope"] != scope)
    {
        return Err(HostError::Execution(
            "provider_continuation_scope_mismatch".into(),
        ));
    }
    context
        .store
        .bind_run(
            &context.task.task_id,
            chat_id,
            scope,
            context.fence.clone(),
            context.runtime.now_ms(),
        )
        .await?;
    let mut metadata = records
        .iter()
        .find(|message| message.id == anchor)
        .map(|message| message.metadata.clone())
        .unwrap_or(json!({}));
    metadata["canonicalProviderDisplay"] = true.into();
    context
        .store
        .update_message(
            anchor,
            None,
            Some(metadata),
            Some(context.fence.clone()),
            context.runtime.now_ms(),
        )
        .await?;
    Ok(Some(ContinuationSession {
        state: previous,
        scope: ContinuationScope {
            chat_id: chat_id.into(),
            branch_id: branch.id.clone(),
        },
    }))
}
pub(super) fn assemble(
    records: Vec<MessageRecord>,
    limit: usize,
    anchor: &str,
    system_prompt: Option<String>,
    receipts: &[agentkit_store::ExecutionReceiptRecord],
    run_id: &str,
) -> Vec<ChatMessage> {
    let start = records.len().saturating_sub(limit);
    let ordered = order_messages_for_provider(records.into_iter().skip(start).collect());
    let mut messages = vec![];
    if let Some(prompt) = system_prompt.filter(|prompt| !prompt.is_empty()) {
        messages.push(ChatMessage::text("system", &prompt));
    }
    let mut declared = HashSet::new();
    for record in ordered {
        // An inactive record is normally not part of the active chain —
        // except one written BY THIS RUN (a denial, an answered human
        // request, an approval mismatch): those are internal, deliberately
        // unactivated, and still answer this run's calls (the TypeScript
        // anchored-history rule).
        if !record.active
            && !(record.run_id.as_deref() == Some(run_id) && record.metadata["internal"] == true)
        {
            continue;
        }
        if record.id == anchor
            || record.metadata["placeholder"] == true
            || record.metadata["canonicalProviderDisplay"] == true
            || record.role == "system"
            || record.metadata.get("correctionPass").is_some()
                && record.metadata["canonicalProviderTurn"] != true
        {
            continue;
        }
        let calls: Vec<ToolCall> = record
            .tool_calls
            .and_then(|value| serde_json::from_value(value).ok())
            .unwrap_or_default();
        if record.role == "assistant" {
            for call in &calls {
                declared.insert(call.id.clone());
            }
        }
        if record.role == "tool"
            && record
                .tool_call_id
                .as_ref()
                .is_none_or(|id| !declared.contains(id))
        {
            continue;
        }
        messages.push(ChatMessage {
            role: record.role,
            content: record.content,
            name: record.metadata["toolName"].as_str().map(str::to_owned),
            tool_call_id: record.tool_call_id,
            tool_calls: calls,
            metadata: None,
        });
    }
    reconcile(
        messages,
        receipts
            .iter()
            // Later inserts overwrite: the last receipt per call id wins.
            .map(|record| (record.tool_call_id.clone(), record))
            .collect(),
    )
}
fn reconcile(
    messages: Vec<ChatMessage>,
    receipts: std::collections::HashMap<String, &agentkit_store::ExecutionReceiptRecord>,
) -> Vec<ChatMessage> {
    let mut answered: HashSet<String> = messages
        .iter()
        .filter(|message| message.role == "tool")
        .filter_map(|message| message.tool_call_id.clone())
        .collect();
    let mut result = vec![];
    for message in messages {
        let calls = message.tool_calls.clone();
        result.push(message);
        for call in calls {
            if answered.insert(call.id.clone()) {
                // Reconcile against the durable receipt before falling back
                // to the generic synthetic failure (ADR 0020): a completed
                // receipt replays its recorded result; an admitted receipt
                // is an uncertain effect that must not be reissued.
                let (text, code) = match receipts.get(&call.id) {
                    Some(record)
                        if record.status == agentkit_store::ExecutionReceiptStatus::Completed =>
                    {
                        match record
                            .result
                            .as_ref()
                            .and_then(|value| serde_json::from_value::<agentkit_core::ToolResult>(value.clone()).ok())
                        {
                            Some(stored) => (
                                agentkit_core::tool_model_envelope(&stored, &stored.limits),
                                None,
                            ),
                            None => (
                                "admitted execution has no durable result; uncertain side effect, do not reissue".into(),
                                Some("tool_effect_uncertain"),
                            ),
                        }
                    }
                    Some(_) => (
                        "admitted execution has no durable result; uncertain side effect, do not reissue"
                            .into(),
                        Some("tool_effect_uncertain"),
                    ),
                    None => (
                        "missing tool result for persisted tool call; reconciled as synthetic failure"
                            .into(),
                        Some("tool_result_missing"),
                    ),
                };
                let envelope = match code {
                    None => text,
                    Some(code) => json!({"ok":false,"status":"error","summary":text,"warnings":[],"truncated":false,"data":{"errorCode":code,"errorMessage":text}})
                        .to_string(),
                };
                let mut missing = ChatMessage::text("tool", &envelope);
                missing.tool_call_id = Some(call.id);
                missing.name = Some(call.name);
                result.push(missing);
            }
        }
    }
    result
}
pub(super) struct DurableEvents {
    pub context: ExecutionContext,
    pub chat_id: String,
    pub anchor: String,
    pub provider_id: String,
    pub canonical: bool,
    pub display_metadata: Value,
    pub usage: Option<Arc<dyn UsageAuthorizer>>,
    pub usage_timeout_ms: u64,
}
#[async_trait]
impl RunEventSink for DurableEvents {
    async fn append(&self, event: &agentkit_contracts::AiRunEvent) -> Result<(), CoreError> {
        self.persist(event, &[]).await
    }
    async fn append_messages(
        &self,
        event: &agentkit_contracts::AiRunEvent,
        messages: &[ChatMessage],
    ) -> Result<(), CoreError> {
        self.persist(event, messages).await
    }
}
impl DurableEvents {
    async fn persist(
        &self,
        event: &agentkit_contracts::AiRunEvent,
        messages: &[ChatMessage],
    ) -> Result<(), CoreError> {
        let event = serde_json::to_value(event).map_err(|_| {
            CoreError::new("event_serialization_failed", "event serialization failed")
        })?;
        if event["type"] == "run.input.applied" {
            return self.persist_steered(&event).await;
        }
        let mut projected = vec![];
        for message in messages {
            let final_answer = message.role == "assistant" && message.tool_calls.is_empty();
            let mut metadata = json!({});
            if self.canonical {
                metadata["canonicalProviderTurn"] = true.into();
            }
            if final_answer && !self.canonical {
                metadata = self.display_metadata.clone();
                metadata["placeholder"] = true.into();
            }
            if !final_answer {
                metadata["internal"] = true.into();
            }
            if let Some(name) = &message.name {
                metadata["toolName"] = name.clone().into();
            }
            let input = MessageInput {
                id: Some(if final_answer && !self.canonical {
                    self.anchor.clone()
                } else {
                    self.context.runtime.id()
                }),
                chat_id: self.chat_id.clone(),
                run_id: Some(self.context.task.task_id.clone()),
                role: message.role.clone(),
                content: message.content.clone(),
                tool_call_id: message.tool_call_id.clone(),
                tool_calls: if message.tool_calls.is_empty() {
                    None
                } else {
                    Some(serde_json::to_value(&message.tool_calls).expect("JSON only"))
                },
                model_result_json: if message.role == "tool" {
                    message.content.as_str().map(str::to_owned)
                } else {
                    None
                },
                metadata,
                ..MessageInput::default()
            };
            projected.push(input);
            if final_answer && self.canonical {
                projected.push(MessageInput {
                    id: Some(self.anchor.clone()),
                    chat_id: self.chat_id.clone(),
                    run_id: Some(self.context.task.task_id.clone()),
                    role: "assistant".into(),
                    content: message.content.clone(),
                    metadata: {
                        let mut metadata = self.display_metadata.clone();
                        metadata["canonicalProviderDisplay"] = true.into();
                        metadata["placeholder"] = true.into();
                        metadata
                    },
                    ..MessageInput::default()
                });
            }
        }
        self.context
            .store
            .append_projected(
                self.context.fence.clone(),
                vec![event.clone()],
                projected,
                self.context.runtime.now_ms(),
            )
            .await
            .map_err(|_| CoreError::new("event_persistence_failed", "event persistence failed"))?;
        if event["type"] == "run.usage"
            && let Some(usage) = &self.usage
        {
            let mut record = event["data"].clone();
            record["providerId"] = self.provider_id.clone().into();
            record["runId"] = self.context.task.task_id.clone().into();
            record["at"] = event["timestamp"].clone();
            // Durable usage remains authoritative when an external meter is unavailable.
            let _ = super::hooks::bounded(
                usage.record(&record),
                self.context.cancellation.clone(),
                self.usage_timeout_ms,
                "usage.record",
            )
            .await;
        }
        Ok(())
    }
}

impl DurableEvents {
    /// The projection of a placed steer (ADR 0016 §3): the durable input's
    /// content becomes a user record and the input is receipted APPLIED —
    /// in ONE transaction with the event, so the log and the history never
    /// disagree about what the model saw. An input that vanished or carries
    /// unreadable content costs the message only; the event still lands.
    async fn persist_steered(&self, event: &Value) -> Result<(), CoreError> {
        let input_id = event["data"]["inputId"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let record = self
            .context
            .store
            .get_input(input_id.clone())
            .await
            .map_err(|_| CoreError::new("event_persistence_failed", "event persistence failed"))?;
        let content = record
            .as_ref()
            .and_then(|record| record.content.get("content"))
            .filter(|content| content.is_string() || content.is_array())
            .cloned();
        let (messages, applies) = match content {
            Some(content) => {
                let message_id = self.context.runtime.id();
                (
                    vec![MessageInput {
                        id: Some(message_id.clone()),
                        chat_id: self.chat_id.clone(),
                        run_id: Some(self.context.task.task_id.clone()),
                        role: "user".into(),
                        content,
                        metadata: json!({"internal": true, "steering": true, "inputId": input_id}),
                        ..MessageInput::default()
                    }],
                    vec![agentkit_store::AppliedSteer {
                        input_id,
                        message_id,
                    }],
                )
            }
            None => (vec![], vec![]),
        };
        self.context
            .store
            .append_projected_inputs(agentkit_store::AppendProjectedInputs {
                fence: self.context.fence.clone(),
                events: vec![event.clone()],
                messages,
                creates: vec![],
                applies,
                task_id: self.context.task.task_id.clone(),
                now: self.context.runtime.now_ms(),
            })
            .await
            .map_err(|_| CoreError::new("event_persistence_failed", "event persistence failed"))?;
        Ok(())
    }
}
