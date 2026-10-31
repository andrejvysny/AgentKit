use super::{
    compaction::{self, Entry},
    compaction_request,
    context_budget::{self, ContextBudget},
};
use crate::{ExecutionContext, HostError, HostResult};
use agentkit_core::{ChatProvider, ChatRequest, RunOptions, Terminal, ToolRegistry};
use futures::StreamExt;
use serde_json::{Value, json};
use std::sync::Arc;

pub(super) struct Input {
    pub context: ExecutionContext,
    pub chat_id: String,
    pub model: String,
    pub provider: Arc<dyn ChatProvider>,
    pub request_budget: Arc<agentkit_core::ExecutionBudget>,
    pub context_budget: ContextBudget,
    pub entries: Vec<Entry>,
    pub tools: Vec<agentkit_core::ToolDefinition>,
    pub path_snapshot: Value,
    pub receipt_snapshot: Value,
    pub usage: Option<Arc<dyn crate::UsageAuthorizer>>,
    pub usage_timeout_ms: u64,
}

struct Snapshot {
    marker: Option<Value>,
    descriptor: Option<Value>,
    path: Value,
    receipts: Value,
    summaries: Vec<Value>,
}
impl Snapshot {
    async fn capture(input: &Input) -> HostResult<Self> {
        let chat = input
            .context
            .store
            .get_chat(&input.chat_id)
            .await?
            .ok_or_else(|| HostError::Execution("session not found".into()))?;
        let summaries = input
            .context
            .store
            .list_context_summaries(&input.chat_id)
            .await?;
        Ok(Self {
            marker: chat
                .metadata
                .get(agentkit_store::COMPACT_REQUESTED_METADATA_KEY)
                .cloned(),
            descriptor: chat.metadata.get("agentkit.session").cloned(),
            path: input.path_snapshot.clone(),
            receipts: input.receipt_snapshot.clone(),
            summaries,
        })
    }
    fn manual(&self) -> bool {
        self.marker.as_ref().is_some_and(Value::is_string)
    }
}
struct Plan {
    working: Vec<Entry>,
    previous: Option<Value>,
    body_start: usize,
    from: usize,
    cut: usize,
    budget: u64,
    triggered: bool,
}
fn make_plan(input: &Input, snapshot: &Snapshot) -> Plan {
    let found = snapshot.summaries.iter().find_map(|record| {
        compaction::apply_summary(&input.entries, record).map(|working| (record.clone(), working))
    });
    let (previous, working) = match found {
        Some((record, working)) => (Some(record), working),
        None => (None, input.entries.clone()),
    };
    let budget = message_budget(input);
    let tokens = working
        .iter()
        .map(|entry| context_budget::message_tokens(&entry.0))
        .sum::<u64>();
    let body_start = usize::from(
        input
            .entries
            .first()
            .is_some_and(|entry| entry.0.role == "system"),
    );
    let body = &input.entries[body_start..];
    let from = previous
        .as_ref()
        .and_then(|p| {
            body.iter()
                .position(|entry| entry.1.as_deref() == p["firstKeptMessageId"].as_str())
        })
        .unwrap_or(0);
    let keep = input
        .context
        .task
        .payload
        .get("sessionContext")
        .or_else(|| snapshot.descriptor.as_ref().and_then(|d| d.get("context")))
        .and_then(|c| c["keepRecentTokens"].as_u64())
        .unwrap_or(20_000);
    let cut = compaction::find_cut(body, from, keep);
    Plan {
        working,
        previous,
        body_start,
        from,
        cut,
        budget,
        triggered: snapshot.manual() || tokens > budget,
    }
}
fn message_budget(input: &Input) -> u64 {
    let tools = serde_json::to_string(&input.tools)
        .unwrap_or_default()
        .encode_utf16()
        .count() as u64;
    input
        .context_budget
        .window
        .saturating_sub(input.context_budget.reserve)
        .saturating_sub(if input.tools.is_empty() {
            0
        } else {
            tools.div_ceil(4)
        })
}
pub(super) async fn apply(input: Input) -> HostResult<Vec<Entry>> {
    let snapshot = Snapshot::capture(&input).await?;
    let plan = make_plan(&input, &snapshot);
    if !plan.triggered {
        return Ok(plan.working);
    }
    if plan.cut <= plan.from {
        if snapshot.manual() {
            commit(&input, &snapshot, None).await?;
        }
        return trim_and_report(&input.context, plan.working, plan.budget).await;
    }
    let body = &input.entries[plan.body_start..];
    let id = format!(
        "{}:compaction:{}",
        input.context.task.task_id,
        snapshot.summaries.len() + 1
    );
    let (result, usage) = summarize(
        &input,
        &body[plan.from..plan.cut],
        plan.previous.as_ref(),
        &id,
    )
    .await;
    emit_usage(&input, usage).await?;
    let working = match result {
        Ok(text) => save_summary(&input, &snapshot, &plan, &id, &text).await?,
        Err(reason) => {
            input.context.check_cancelled()?;
            input.context.emit("run.warning", json!({"code":"compaction_failed","message":format!("Context compaction failed ({reason}); the history was trimmed instead.")})).await?;
            plan.working.clone()
        }
    };
    trim_and_report(&input.context, working, plan.budget).await
}
async fn save_summary(
    input: &Input,
    snapshot: &Snapshot,
    plan: &Plan,
    id: &str,
    text: &str,
) -> HostResult<Vec<Entry>> {
    let body = &input.entries[plan.body_start..];
    let (details, commands) =
        compaction_request::file_details(&body[plan.from..plan.cut], plan.previous.as_ref());
    let summary = compaction_request::add_details(text, &details, &commands);
    let record = record(input, body, plan.cut, id, &summary, details)?;
    commit(input, snapshot, Some(record.clone())).await?;
    input.context.emit("run.context.compacted", json!({"summaryId":id,"tokensBefore":record["tokensBefore"],"tokensAfter":record["tokensAfter"],"coveredMessages":plan.cut,"reason":if snapshot.manual() {"manual"} else {"threshold"}})).await?;
    Ok(compaction::apply_summary(&input.entries, &record).unwrap_or_else(|| plan.working.clone()))
}
async fn emit_usage(input: &Input, usage: Vec<Value>) -> HostResult<()> {
    account_all(usage, |item| account_one(input, item)).await
}
async fn account_all<F, Fut>(usage: Vec<Value>, mut account: F) -> HostResult<()>
where
    F: FnMut(Value) -> Fut,
    Fut: std::future::Future<Output = HostResult<()>>,
{
    let mut first_error = None;
    for item in usage {
        if let Err(error) = account(item).await
            && first_error.is_none()
        {
            first_error = Some(error);
        }
    }
    first_error.map_or(Ok(()), Err)
}
async fn account_one(input: &Input, mut item: Value) -> HostResult<()> {
    item["step"] = 0.into();
    let recorded = input.context.emit("run.usage", item.clone()).await;
    if let Some(meter) = &input.usage {
        item["providerId"] = input.provider.id().into();
        item["runId"] = input.context.task.task_id.clone().into();
        item["at"] = agentkit_store::ConversationTimestamp::from_epoch_millis(
            input.context.runtime.now_ms(),
        )
        .ok()
        .and_then(|value| value.wire_text().ok().map(|text| text.into_owned()))
        .unwrap_or_default()
        .into();
        let _ = super::hooks::bounded(
            meter.record(&item),
            tokio_util::sync::CancellationToken::new(),
            input.usage_timeout_ms,
            "usage.record",
        )
        .await;
    }
    recorded
}
async fn summarize(
    input: &Input,
    covered: &[Entry],
    previous: Option<&Value>,
    id: &str,
) -> (Result<String, &'static str>, Vec<Value>) {
    let Some((messages, max_output_tokens)) = compaction_request::summary_messages(
        covered,
        previous,
        input.context_budget.window,
        input.context_budget.reserve,
    ) else {
        return (Err("summarizer context overflow"), vec![]);
    };
    let cancellation = input.context.cancellation.child_token();
    let mut handle = start_summary(input, id, messages, max_output_tokens, cancellation.clone());
    let (usage, finish, error) =
        collect_events(&mut handle, std::time::Duration::from_secs(60)).await;
    if let Some(error) = error {
        cancellation.cancel();
        handle.result.abort();
        return (Err(error), usage);
    }
    let result = handle
        .result
        .await
        .map_err(|_| "summarizer request threw")
        .and_then(|result| summary_text(result, &finish));
    (result, usage)
}

fn start_summary(
    input: &Input,
    id: &str,
    messages: Vec<agentkit_core::ChatMessage>,
    max_output_tokens: u64,
    cancellation: tokio_util::sync::CancellationToken,
) -> agentkit_core::RunHandle {
    let request = ChatRequest {
        model: input.model.clone(),
        messages,
        max_output_tokens: Some(max_output_tokens),
        budget: Some(input.request_budget.clone()),
        ..Default::default()
    };
    let options = RunOptions {
        request,
        run_id: Some(id.into()),
        max_tool_iterations: 1,
        cancellation,
        ..Default::default()
    };
    agentkit_core::run_chat(
        options,
        input.provider.clone(),
        Arc::new(ToolRegistry::new()),
    )
}
async fn collect_events(
    handle: &mut agentkit_core::RunHandle,
    timeout: std::time::Duration,
) -> (Vec<Value>, String, Option<&'static str>) {
    let mut usage = vec![];
    let mut finish = String::new();
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let event = match tokio::time::timeout_at(deadline, handle.events.next()).await {
            Ok(Some(event)) => event,
            Ok(None) => break,
            Err(_) => return (usage, finish, Some("summarizer timed out")),
        };
        let Ok(value) = serde_json::to_value(event) else {
            return (usage, finish, Some("invalid summarizer event"));
        };
        match value["type"].as_str() {
            Some("run.usage") => usage.push(value["data"].clone()),
            Some("run.completed") => {
                finish = value["data"]["finishReason"]
                    .as_str()
                    .unwrap_or_default()
                    .into()
            }
            _ => {}
        }
    }
    (usage, finish, None)
}
fn summary_text(result: agentkit_core::RunResult, finish: &str) -> Result<String, &'static str> {
    if result.terminal != Terminal::Completed {
        return Err("summarizer failed");
    }
    if finish == "length" || finish == "incomplete" {
        return Err("summary incomplete");
    }
    let text = result
        .appended_messages
        .iter()
        .rev()
        .find(|message| message.role == "assistant")
        .map(|message| super::hooks::message_text(&message.content))
        .unwrap_or_default()
        .trim()
        .to_owned();
    if text.is_empty() {
        return Err("summarizer returned no text");
    }
    Ok(text)
}

fn record(
    input: &Input,
    body: &[Entry],
    cut: usize,
    id: &str,
    summary: &str,
    details: Value,
) -> HostResult<Value> {
    let timestamp =
        agentkit_store::ConversationTimestamp::from_epoch_millis(input.context.runtime.now_ms())
            .and_then(|value| value.wire_text().map(|text| text.into_owned()))
            .map_err(|_| HostError::Execution("invalid runtime timestamp".into()))?;
    let through = body[cut - 1]
        .1
        .as_deref()
        .ok_or_else(|| HostError::Execution("summary source id missing".into()))?;
    let kept = body[cut]
        .1
        .as_deref()
        .ok_or_else(|| HostError::Execution("summary source id missing".into()))?;
    let hash = compaction::hash_source(&body[..cut])
        .ok_or_else(|| HostError::Execution("summary source hash failed".into()))?;
    let before: u64 = body[..cut]
        .iter()
        .map(|entry| context_budget::message_tokens(&entry.0))
        .sum();
    let after = context_budget::message_tokens(&agentkit_core::ChatMessage::text(
        "user",
        &format!(
            "The conversation history before this point was compacted into the following summary:\n<summary>\n{summary}\n</summary>"
        ),
    ));
    Ok(
        json!({"summaryId":id,"sessionId":input.chat_id,"coversThroughMessageId":through,"firstKeptMessageId":kept,"summary":summary,"sourceHash":hash,"tokensBefore":before,"tokensAfter":after,"providerId":input.provider.id(),"model":input.model,"summarizerRevision":compaction::REVISION,"details":details,"createdAt":timestamp}),
    )
}

async fn commit(input: &Input, snapshot: &Snapshot, record: Option<Value>) -> HostResult<()> {
    let tx = input.context.store.begin_transaction().await?;
    let view = tx.view();
    verify_active(input, &view).await?;
    let chat = validate_snapshot(input, snapshot, &view).await?;
    if let Some(record) = record {
        view.save_context_summary(record).await?;
    }
    if snapshot.manual() {
        let mut metadata = chat.metadata;
        if let Some(map) = metadata.as_object_mut() {
            map.remove(agentkit_store::COMPACT_REQUESTED_METADATA_KEY);
        }
        view.update_chat(
            &input.chat_id,
            None,
            Some(metadata),
            None,
            input.context.runtime.now_ms(),
        )
        .await?;
    }
    view.verify_fence(&input.context.fence, input.context.runtime.now_ms())
        .await?;
    input.context.check_cancelled()?;
    tx.commit().await?;
    Ok(())
}
async fn verify_active(input: &Input, view: &agentkit_store::SqliteStore) -> HostResult<()> {
    input.context.check_cancelled()?;
    view.verify_fence(&input.context.fence, input.context.runtime.now_ms())
        .await?;
    let task = view
        .get_task(&input.context.task.task_id)
        .await?
        .ok_or_else(|| HostError::Execution("compaction task disappeared".into()))?;
    if task.cancel_requested {
        return Err(HostError::Cancelled);
    }
    if task.interrupt_requested {
        return Err(HostError::Execution("compaction interrupted".into()));
    }
    Ok(())
}
async fn validate_snapshot(
    input: &Input,
    snapshot: &Snapshot,
    view: &agentkit_store::SqliteStore,
) -> HostResult<agentkit_store::ChatRecord> {
    let chat = view
        .get_chat(&input.chat_id)
        .await?
        .ok_or_else(|| HostError::Execution("session disappeared".into()))?;
    let path = serde_json::to_value(view.messages(&input.chat_id, true).await?)
        .map_err(|_| HostError::Execution("context snapshot failed".into()))?;
    let receipts = serde_json::to_value(view.list_executions_by_chat(input.chat_id.clone()).await?)
        .map_err(|_| HostError::Execution("receipt snapshot failed".into()))?;
    let summaries = view.list_context_summaries(&input.chat_id).await?;
    if chat.metadata.get("agentkit.session") != snapshot.descriptor.as_ref()
        || chat
            .metadata
            .get(agentkit_store::COMPACT_REQUESTED_METADATA_KEY)
            != snapshot.marker.as_ref()
        || path != snapshot.path
        || receipts != snapshot.receipts
        || summaries != snapshot.summaries
    {
        return Err(HostError::Execution(
            "context changed during compaction".into(),
        ));
    }
    Ok(chat)
}

async fn trim_and_report(
    context: &ExecutionContext,
    mut entries: Vec<Entry>,
    budget: u64,
) -> HostResult<Vec<Entry>> {
    let tokens = |entries: &[Entry]| {
        entries
            .iter()
            .map(|entry| context_budget::message_tokens(&entry.0))
            .sum::<u64>()
    };
    let system = usize::from(
        entries
            .first()
            .is_some_and(|entry| entry.0.role == "system"),
    );
    let mut dropped = 0;
    while tokens(&entries) > budget {
        let next = (system + 1..entries.len())
            .find(|&i| entries[i].0.role == "user" && entries[i - 1].0.role != "tool");
        let Some(next) = next else {
            break;
        };
        entries.drain(system..next);
        dropped += 1;
    }
    if dropped > 0 {
        context.emit("run.warning", json!({"code":"context_trimmed","message":format!("Dropped the {dropped} oldest turn(s) to fit the context window; about {} of {budget} budgeted tokens remain in use.", tokens(&entries))})).await?;
    }
    Ok(entries)
}

#[cfg(test)]
#[path = "compaction_runner_tests.rs"]
mod tests;
