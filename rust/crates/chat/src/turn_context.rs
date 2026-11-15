//! Per-turn context line (T4.4): the default [`TurnContextHook`] recommends
//! toolsets with the core `Recommender` over the session's toolset view,
//! ranked also by usage counted from `run.tool.record` events.
use crate::{TurnContextHook, TurnContextInput};
use agentkit_contracts::AiRunToolRecordEventData;
use agentkit_core::{
    ChatMessage, InMemoryUsage, META_TOOLSET, Recommender, ToolRegistry, UsageStore, docs_from_view,
    record_usage,
};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::Arc;

/// Advises which toolsets look useful for the user's request. It only
/// writes one line; it never removes a tool. Usage is counted per chat.
#[derive(Default)]
pub struct ToolsetRecommendation {
    usage: Arc<InMemoryUsage>,
}

impl ToolsetRecommendation {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl TurnContextHook for ToolsetRecommendation {
    async fn line(&self, input: &TurnContextInput<'_>) -> Option<String> {
        let docs = docs_from_view(input.view, input.tools);
        if docs.is_empty() || input.request.trim().is_empty() {
            return None;
        }
        let loaded: Vec<String> = input
            .tools
            .iter()
            .map(|definition| input.view.id_of(definition))
            .filter(|id| id != META_TOOLSET)
            .collect();
        let store: Arc<dyn UsageStore> = self.usage.clone();
        Recommender::new(docs)
            .with_usage(store, input.chat_id)
            .recommend(input.request, &loaded)
            .line()
    }

    fn observe_tool_record(&self, chat_id: &str, record: &AiRunToolRecordEventData) {
        record_usage(self.usage.as_ref(), chat_id, record);
    }
}

fn user_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Appends the hook's line to the turn's last user message. The message in
/// the durable store is untouched, so the line is never replayed as history.
/// A turn that ends in a tool result (a resume) gets no line.
pub(crate) async fn append_line(
    hook: &dyn TurnContextHook,
    chat_id: &str,
    registry: &ToolRegistry,
    messages: &mut [ChatMessage],
) {
    let Some(last) = messages.last_mut().filter(|m| m.role == "user") else {
        return;
    };
    let request = user_text(&last.content);
    let tools = registry.definitions();
    let view = registry.toolset_view(&tools, None);
    let input = TurnContextInput {
        chat_id,
        request: &request,
        tools: &tools,
        view: &view,
    };
    let Some(line) = hook.line(&input).await.filter(|line| !line.is_empty()) else {
        return;
    };
    match &mut last.content {
        Value::String(text) => {
            text.push_str("\n\n");
            text.push_str(&line);
        }
        Value::Array(parts) => parts.push(json!({"type": "text", "text": line})),
        _ => {}
    }
}
