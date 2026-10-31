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
#[cfg(test)]
pub(super) fn assemble(
    records: Vec<MessageRecord>,
    limit: usize,
    anchor: &str,
    system_prompt: Option<String>,
    receipts: &[agentkit_store::ExecutionReceiptRecord],
    run_id: &str,
) -> Vec<ChatMessage> {
    assemble_with_ids(records, limit, anchor, system_prompt, receipts, run_id)
        .into_iter()
        .map(|(message, _)| message)
        .collect()
}

pub(super) fn assemble_with_ids(
    records: Vec<MessageRecord>,
    limit: usize,
    anchor: &str,
    system_prompt: Option<String>,
    receipts: &[agentkit_store::ExecutionReceiptRecord],
    run_id: &str,
) -> Vec<(ChatMessage, Option<String>)> {
    let ordered = order_messages_for_provider(records);
    let eligible = eligible_records(&ordered, anchor, run_id);
    let start = history_start(&eligible, limit);
    let mut messages = vec![];
    if let Some(prompt) = system_prompt.filter(|prompt| !prompt.is_empty()) {
        messages.push((ChatMessage::text("system", &prompt), None, None));
    }
    let declared = declared_calls(&eligible[start..]);
    for record in eligible.into_iter().skip(start) {
        if let Some(message) = visible_message(record, &declared) {
            messages.push(message);
        }
    }
    reconcile(messages, receipts)
}

// Filter the active path before limiting so inactive siblings do not consume the window.
fn eligible_records<'a>(
    ordered: &'a [MessageRecord],
    anchor: &str,
    run_id: &str,
) -> Vec<&'a MessageRecord> {
    ordered
        .iter()
        .filter(|record| {
            record.active
                || (record.run_id.as_deref() == Some(run_id) && record.metadata["internal"] == true)
        })
        .filter(|record| {
            record.id != anchor
                && record.metadata["placeholder"] != true
                && record.metadata["canonicalProviderDisplay"] != true
                && record.role != "system"
                && !(record.metadata.get("correctionPass").is_some()
                    && record.metadata["canonicalProviderTurn"] != true)
        })
        .collect()
}

// Extend the cut when necessary to keep each tool result with its declaration.
fn history_start(eligible: &[&MessageRecord], limit: usize) -> usize {
    let mut start = eligible.len().saturating_sub(limit);
    while start > 0 {
        let declared = declared_calls(&eligible[start..]);
        let opens_in_group = eligible[start].role == "tool"
            && !eligible[start]
                .tool_call_id
                .as_ref()
                .is_none_or(|id| declared.contains(&(eligible[start].run_id.clone(), id.clone())));
        if !opens_in_group {
            break;
        }
        start -= 1;
    }
    start
}

fn declared_calls(records: &[&MessageRecord]) -> HashSet<(Option<String>, String)> {
    records
        .iter()
        .filter(|record| record.role == "assistant")
        .flat_map(|record| {
            record
                .tool_calls
                .as_ref()
                .and_then(|value| serde_json::from_value::<Vec<ToolCall>>(value.clone()).ok())
                .unwrap_or_default()
                .into_iter()
                .map(move |call| (record.run_id.clone(), call.id))
        })
        .collect()
}

type OwnedHistoryMessage = (ChatMessage, Option<String>, Option<String>);
fn visible_message(
    record: &MessageRecord,
    declared: &HashSet<(Option<String>, String)>,
) -> Option<OwnedHistoryMessage> {
    if record.role == "tool"
        && record
            .tool_call_id
            .as_ref()
            .is_none_or(|id| !declared.contains(&(record.run_id.clone(), id.clone())))
    {
        return None;
    }
    let calls: Vec<ToolCall> = record
        .tool_calls
        .as_ref()
        .and_then(|value| serde_json::from_value(value.clone()).ok())
        .unwrap_or_default();
    Some((
        ChatMessage {
            role: record.role.clone(),
            content: record.content.clone(),
            name: record.metadata["toolName"].as_str().map(str::to_owned),
            tool_call_id: record.tool_call_id.clone(),
            tool_calls: calls,
            metadata: None,
        },
        record.run_id.clone(),
        Some(record.id.clone()),
    ))
}
/// Resolve unanswered persisted calls against the durable receipts.
/// Identity is at least `(run_id, tool_call_id)`: tool-call ids are stable
/// per run (ADR 0020) and a provider may reuse one across runs, so a
/// receipt from another run is different evidence, never this call's
/// answer. An exact-keyed receipt must also match the declaring call's
/// tool name and canonical arguments; conflicting evidence resolves as
/// UNCERTAIN, never as another operation's success.
enum ReceiptDecision {
    /// A completed, identity-matched receipt: replay its recorded result.
    Completed(String),
    /// Admitted-without-result, or evidence that conflicts with the
    /// declaring call (different run's id reuse excluded from matching).
    Uncertain,
    /// No receipt ever recorded for this (run, call): the body never
    /// started; honest synthetic failure.
    Missing,
}
fn resolve_receipt(
    receipts: &[agentkit_store::ExecutionReceiptRecord],
    owner_run: Option<&String>,
    call: &ToolCall,
) -> ReceiptDecision {
    let Some(owner_run) = owner_run else {
        // A record without a run cannot own durable execution evidence.
        return ReceiptDecision::Missing;
    };
    // Newest insert wins for this exact operation identity: a newer
    // admitted attempt reads as uncertain even when an older attempt of
    // the same (run, call) completed.
    let Some(record) = receipts
        .iter()
        .rev()
        .find(|record| &record.run_id == owner_run && record.tool_call_id == call.id)
    else {
        return ReceiptDecision::Missing;
    };
    // Identity validation beyond the key: same tool, same canonical
    // arguments — hashed with the same JavaScript-semantic parser the
    // admission used, or `1.0`-style spellings would fake a conflict and
    // downgrade a completed receipt to uncertain. Conflicting evidence is
    // uncertainty, not success.
    let arguments_match = agentkit_contracts::parse_javascript_json(&call.arguments_json)
        .ok()
        .is_some_and(|arguments| crate::governance::args_sha256(&arguments) == record.args_sha256);
    if record.tool_name != call.name || !arguments_match {
        return ReceiptDecision::Uncertain;
    }
    if record.status == agentkit_store::ExecutionReceiptStatus::Completed {
        return match record.result.as_ref().and_then(|value| {
            serde_json::from_value::<agentkit_core::ToolResult>(value.clone()).ok()
        }) {
            Some(stored) => ReceiptDecision::Completed(agentkit_core::tool_model_envelope(
                &stored,
                &stored.limits,
            )),
            None => ReceiptDecision::Uncertain,
        };
    }
    ReceiptDecision::Uncertain
}
fn reconcile(
    messages: Vec<(ChatMessage, Option<String>, Option<String>)>,
    receipts: &[agentkit_store::ExecutionReceiptRecord],
) -> Vec<(ChatMessage, Option<String>)> {
    let mut answered: HashSet<(Option<String>, String)> = messages
        .iter()
        .filter(|(message, _, _)| message.role == "tool")
        .filter_map(|(message, owner, _)| {
            message.tool_call_id.clone().map(|id| (owner.clone(), id))
        })
        .collect();
    let mut result = vec![];
    for (message, owner, record_id) in messages {
        let calls = message.tool_calls.clone();
        result.push((message, record_id.clone()));
        for call in calls {
            if answered.insert((owner.clone(), call.id.clone())) {
                result.push(synthetic_result(
                    receipts,
                    owner.as_ref(),
                    record_id.as_deref(),
                    call,
                ));
            }
        }
    }
    result
}
fn synthetic_result(
    receipts: &[agentkit_store::ExecutionReceiptRecord],
    owner: Option<&String>,
    record_id: Option<&str>,
    call: ToolCall,
) -> (ChatMessage, Option<String>) {
    let (text, code) = match resolve_receipt(receipts, owner, &call) {
        ReceiptDecision::Completed(model_json) => (model_json, None),
        ReceiptDecision::Uncertain => (
            "admitted execution has no durable result; uncertain side effect, do not reissue"
                .into(),
            Some("tool_effect_uncertain"),
        ),
        ReceiptDecision::Missing => (
            "missing tool result for persisted tool call; reconciled as synthetic failure".into(),
            Some(agentkit_core::TOOL_RESULT_MISSING_CODE),
        ),
    };
    let envelope = match code {
        None => text,
        Some(code) => json!({"ok":false,"status":"error","summary":text,"warnings":[],"truncated":false,"data":{"errorCode":code,"errorMessage":text}}).to_string(),
    };
    let mut message = ChatMessage::text("tool", &envelope);
    let id = record_id.map(|id| format!("{id}#missing:{}", call.id));
    message.tool_call_id = Some(call.id);
    message.name = Some(call.name);
    (message, id)
}
pub(super) struct DurableEvents {
    pub context: ExecutionContext,
    pub chat_id: String,
    pub anchor: String,
    pub provider_id: String,
    pub fenced_tools: std::collections::HashSet<String>,
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
        let mut event = serde_json::to_value(event).map_err(|_| {
            CoreError::new("event_serialization_failed", "event serialization failed")
        })?;
        if event["type"] == "run.tool.running"
            && event["data"]["toolName"]
                .as_str()
                .is_some_and(|name| self.fenced_tools.contains(name))
        {
            event["data"]["admissionProtocol"] = "fenced-v1".into();
        }
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
    /// The steer event's place in the durable log (ADR 0016 §3). The
    /// PLACEMENT owner is `SessionSteering::pending`: it committed the
    /// input transition, the single user record and the applied receipt in
    /// one transaction before the run ever saw the item. This path is an
    /// observer — it persists the event and NEVER re-projects the message
    /// (a second record for the same input would show the model a
    /// steering that happened once). Because this transaction runs AFTER
    /// the placement transaction, it may be lost to a crash while the
    /// placement stands: the placement pair is the unit of record and the
    /// event is its projection, so `session_run::backfill_applied_steers`
    /// re-derives the missing announcement from the durable input + record
    /// at the start of every later attempt of this run.
    async fn persist_steered(&self, event: &Value) -> Result<(), CoreError> {
        self.context
            .store
            .append_projected_inputs(agentkit_store::AppendProjectedInputs {
                fence: self.context.fence.clone(),
                events: vec![event.clone()],
                messages: vec![],
                creates: vec![],
                applies: vec![],
                task_id: self.context.task.task_id.clone(),
                now: self.context.runtime.now_ms(),
            })
            .await
            .map_err(|_| CoreError::new("event_persistence_failed", "event persistence failed"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentkit_core::{ToolLimits, ToolResult};
    use agentkit_store::{ConversationTimestamp, ExecutionReceiptRecord, ExecutionReceiptStatus};

    fn record(
        id: &str,
        run: Option<&str>,
        role: &str,
        content: Value,
        order_key: u64,
        active: bool,
    ) -> MessageRecord {
        MessageRecord {
            id: id.into(),
            chat_id: "c1".into(),
            run_id: run.map(str::to_owned),
            role: role.into(),
            content,
            parent_message_id: None,
            tool_call_id: None,
            tool_calls: None,
            model_result_json: None,
            metadata: json!({}),
            order_key,
            depth: 0,
            branch_index: 0,
            active,
            created_at: ConversationTimestamp::from_epoch_millis(1_700_000_000_000).unwrap(),
        }
    }

    fn assistant(
        id: &str,
        run: &str,
        order_key: u64,
        calls: &[(&str, &str, &str)],
    ) -> MessageRecord {
        let mut r = record(id, Some(run), "assistant", json!(""), order_key, true);
        // DurableEvents marks every non-final assistant record internal;
        // the provider-ordering helper groups a call with its results on
        // that basis.
        r.metadata = json!({"internal": true});
        r.tool_calls = Some(json!(
            calls
                .iter()
                .map(|(cid, name, args)| json!({"id": cid, "name": name, "argumentsJson": args}))
                .collect::<Vec<_>>()
        ));
        r
    }

    fn tool_result_of(
        id: &str,
        run: &str,
        call_id: &str,
        text: &str,
        order_key: u64,
    ) -> MessageRecord {
        let mut r = record(id, Some(run), "tool", json!(text), order_key, true);
        r.metadata = json!({"internal": true});
        r.tool_call_id = Some(call_id.into());
        r
    }

    fn receipt(
        run: &str,
        call_id: &str,
        tool: &str,
        args: &Value,
        status: ExecutionReceiptStatus,
        result: Option<Value>,
    ) -> ExecutionReceiptRecord {
        ExecutionReceiptRecord {
            execution_id: format!("{run}:{call_id}"),
            run_id: run.into(),
            chat_id: "c1".into(),
            tool_call_id: call_id.into(),
            tool_name: tool.into(),
            args_sha256: crate::governance::args_sha256(args),
            context_sha256: "a".repeat(64),
            attempt_id: "attempt-1".into(),
            approval_request_id: None,
            approval_generation: None,
            status,
            result,
        }
    }

    fn stored_result(data: Value) -> Value {
        serde_json::to_value(ToolResult {
            ok: true,
            data: Some(data),
            sources: vec![],
            warnings: vec![],
            truncated: false,
            limits: ToolLimits::default(),
            model_data: None,
            summary: None,
            status: None,
        })
        .unwrap()
    }

    fn assemble_with(
        records: Vec<MessageRecord>,
        receipts: &[ExecutionReceiptRecord],
        run: &str,
    ) -> Vec<ChatMessage> {
        assemble(records, 32, "anchor", None, receipts, run)
    }

    #[test]
    fn reused_call_id_never_borrows_another_runs_completed_success() {
        // Reproduced B3: run A has an admitted, unresolved call with the
        // SAME tool-call id that run B completed with a different operation.
        // History for run A must report its own uncertainty — never B's
        // success.
        let records = vec![
            record("u1", None, "user", json!("do it"), 1, true),
            assistant(
                "a1",
                "runA",
                2,
                &[("call-1", "mutate_a", "{\"path\":\"a.txt\"}")],
            ),
        ];
        let receipts = vec![
            receipt(
                "runA",
                "call-1",
                "mutate_a",
                &json!({"path": "a.txt"}),
                ExecutionReceiptStatus::Admitted,
                None,
            ),
            receipt(
                "runB",
                "call-1",
                "mutate_b",
                &json!({"path": "b.txt"}),
                ExecutionReceiptStatus::Completed,
                Some(stored_result(json!({"success": "from B"}))),
            ),
        ];
        let messages = assemble_with(records, &receipts, "runA");
        let tool = messages
            .iter()
            .rfind(|m| m.role == "tool")
            .expect("tool message");
        let parsed: Value = serde_json::from_str(tool.content.as_str().unwrap()).unwrap();
        assert_eq!(parsed["ok"], json!(false));
        assert_eq!(parsed["data"]["errorCode"], json!("tool_effect_uncertain"));
        let text = tool.content.as_str().unwrap();
        assert!(
            !text.contains("from B"),
            "another run's success leaked: {text}"
        );
    }

    #[test]
    fn exact_receipt_replays_and_conflicting_identity_is_uncertain() {
        let args = json!({"path": "a.txt"});
        let records = vec![assistant(
            "a1",
            "runA",
            1,
            &[("call-1", "mutate_a", "{\"path\":\"a.txt\"}")],
        )];
        // Identity-matched completion: exact recorded envelope, no error.
        let messages = assemble_with(
            records.clone(),
            &[receipt(
                "runA",
                "call-1",
                "mutate_a",
                &args,
                ExecutionReceiptStatus::Completed,
                Some(stored_result(json!({"ok": true}))),
            )],
            "runA",
        );
        let tool = messages.iter().rfind(|m| m.role == "tool").unwrap();
        let content = tool.content.as_str().unwrap_or_default();
        assert!(!content.contains("tool_effect_uncertain"));
        assert!(!content.contains("tool_result_missing"));
        // Same key, different tool name: conflicting evidence, never success.
        let messages = assemble_with(
            records.clone(),
            &[receipt(
                "runA",
                "call-1",
                "delete_all",
                &args,
                ExecutionReceiptStatus::Completed,
                Some(stored_result(json!({"ok": true}))),
            )],
            "runA",
        );
        let tool = messages.iter().rfind(|m| m.role == "tool").unwrap();
        let parsed: Value = serde_json::from_str(tool.content.as_str().unwrap()).unwrap();
        assert_eq!(parsed["data"]["errorCode"], json!("tool_effect_uncertain"));
        // Same key and tool, different canonical arguments: also uncertain.
        let messages = assemble_with(
            records,
            &[receipt(
                "runA",
                "call-1",
                "mutate_a",
                &json!({"path": "z.txt"}),
                ExecutionReceiptStatus::Completed,
                Some(stored_result(json!({"ok": true}))),
            )],
            "runA",
        );
        let tool = messages.iter().rfind(|m| m.role == "tool").unwrap();
        let parsed: Value = serde_json::from_str(tool.content.as_str().unwrap()).unwrap();
        assert_eq!(parsed["data"]["errorCode"], json!("tool_effect_uncertain"));
    }

    #[test]
    fn newest_receipt_wins_over_older_evidence_for_same_key() {
        let args = json!({});
        let records = vec![assistant("a1", "runA", 1, &[("call-1", "mutate", "{}")])];
        // Older completed, then a newer admitted reissue: the newest state
        // is uncertain even though an older receipt completed.
        let messages = assemble_with(
            records,
            &[
                receipt(
                    "runA",
                    "call-1",
                    "mutate",
                    &args,
                    ExecutionReceiptStatus::Completed,
                    Some(stored_result(json!({"first": true}))),
                ),
                receipt(
                    "runA",
                    "call-1",
                    "mutate",
                    &args,
                    ExecutionReceiptStatus::Admitted,
                    None,
                ),
            ],
            "runA",
        );
        let tool = messages.iter().rfind(|m| m.role == "tool").unwrap();
        let parsed: Value = serde_json::from_str(tool.content.as_str().unwrap()).unwrap();
        assert_eq!(parsed["data"]["errorCode"], json!("tool_effect_uncertain"));
    }

    #[test]
    fn answered_tracking_is_scoped_to_the_owning_run() {
        // Two runs reuse call-1; a tool record for run B answers ONLY run B's
        // declaration. Run A's declaration is reconciled on its own receipts.
        let records = vec![
            assistant("aA", "runA", 1, &[("call-1", "mutate", "{\"n\":1}")]),
            assistant("aB", "runB", 2, &[("call-1", "mutate", "{\"n\":2}")]),
            tool_result_of("tB", "runB", "call-1", "{\"ok\":true}", 3),
        ];
        let messages = assemble_with(records.clone(), &[], "runA");
        // run A's own call gets exactly one synthetic missing; run B's
        // already-answered call gets none.
        let synthesized = messages
            .iter()
            .filter(|m| {
                m.role == "tool"
                    && m.content
                        .as_str()
                        .is_some_and(|text| text.contains("tool_result_missing"))
            })
            .count();
        assert_eq!(synthesized, 1);
        assert!(
            messages
                .iter()
                .any(|m| m.role == "tool" && m.content == json!("{\"ok\":true}"))
        );
    }

    #[test]
    fn history_limit_counts_the_active_path_not_inactive_siblings() {
        // Reproduced D1: two completed active messages plus ten inactive
        // siblings and a new turn. With limit 4 the whole active path fits
        // and must reach the provider.
        let mut records = vec![
            record("u1", None, "user", json!("first question"), 1, true),
            assistant("a1", "run1", 2, &[("c1", "mutate", "{}")]),
            tool_result_of("t1", "run1", "c1", "{\"ok\":true}", 3),
            record("u2", None, "user", json!("second question"), 4, true),
        ];
        for index in 0..10 {
            records.push(record(
                &format!("inactive-{index}"),
                Some("runX"),
                "assistant",
                json!("superseded"),
                10 + index,
                false,
            ));
        }
        let messages = assemble(records, 4, "anchor", None, &[], "runNew");
        let visible: Vec<&str> = messages
            .iter()
            .filter(|m| m.role != "system")
            .map(|m| m.role.as_str())
            .collect();
        assert_eq!(visible, vec!["user", "assistant", "tool", "user"]);
        let texts: Vec<String> = messages.iter().map(|m| m.content.to_string()).collect();
        assert!(
            texts.iter().any(|t| t.contains("first question")),
            "earlier active user message was lost: {texts:?}"
        );
    }

    #[test]
    fn limit_cut_never_splits_an_assistant_from_its_tool_results() {
        let records = vec![
            record("u1", None, "user", json!("q1"), 1, true),
            assistant("a1", "run1", 2, &[("c1", "mutate", "{}")]),
            tool_result_of("t1", "run1", "c1", "{\"ok\":true}", 3),
            record("u2", None, "user", json!("q2"), 4, true),
        ];
        // The limit wants only the last two records (t1, u2); the group
        // rule pulls the declaring assistant in rather than orphaning the
        // tool result or synthesizing a replacement.
        let messages = assemble(records, 2, "anchor", None, &[], "runNew");
        assert!(messages.iter().any(|m| m.role == "assistant"));
        let tool = messages
            .iter()
            .find(|m| m.role == "tool")
            .expect("genuine tool result");
        assert_eq!(tool.content, json!("{\"ok\":true}"));
        assert!(!messages.iter().any(|m| {
            m.content
                .as_str()
                .is_some_and(|t| t.contains("tool_result_missing"))
        }),);
    }
}
