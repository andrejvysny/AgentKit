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
) -> Vec<ChatMessage> {
    let start = records.len().saturating_sub(limit);
    let ordered = order_messages_for_provider(records.into_iter().skip(start).collect());
    let mut messages = vec![];
    if let Some(prompt) = system_prompt.filter(|prompt| !prompt.is_empty()) {
        messages.push(ChatMessage::text("system", &prompt));
    }
    let mut declared = HashSet::new();
    for record in ordered {
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
    reconcile(messages)
}
fn reconcile(messages: Vec<ChatMessage>) -> Vec<ChatMessage> {
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
                let text =
                    "missing tool result for persisted tool call; reconciled as synthetic failure";
                let envelope = json!({"ok":false,"status":"error","summary":text,"warnings":[],"truncated":false,"data":{"errorCode":"tool_result_missing","errorMessage":text}});
                let mut missing = ChatMessage::text("tool", &envelope.to_string());
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
