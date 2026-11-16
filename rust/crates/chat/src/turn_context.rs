//! Per-turn context line (T4.4): the default [`TurnContextHook`] recommends
//! toolsets with the core `Recommender` over the session's toolset view,
//! ranked also by usage counted from `run.tool.record` events.
//!
//! The persisted turn context (roadmap 1.2, ADR 0036) lives in the
//! submodules: `attach` builds it once when a turn starts and stores it on
//! the user message, `render` formats it within its token budget, and
//! `state` records what the history already carries and queues delta notes.
pub(crate) mod attach;
mod render;
pub(crate) mod repair;
mod state;

pub use render::{OversizedItemSink, TURN_CONTEXT_BUDGET_TOKENS, TurnItem, TurnItemKind};
pub use state::{
    TURN_CONTEXT_METADATA_KEY, TurnNote, TurnNoteKind, append_turn_note, with_turn_note,
    without_turn_notes,
};

use crate::{TurnContextHook, TurnContextInput};
use agentkit_contracts::AiRunToolRecordEventData;
use agentkit_core::{
    InMemoryUsage, META_TOOLSET, Recommender, ToolRegistry, UsageStore, docs_from_view,
    record_usage,
};
use async_trait::async_trait;
use serde_json::Value;
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
            // The persisted turn context is not what the user asked.
            .filter(|text| !agentkit_core::is_turn_context_text(text))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// The hook's line as a note of the turn context, `None` when it has nothing
/// to say. The line is written once, when the turn starts, into the persisted
/// turn-context part, so the history replays it unchanged (INV-2); it never
/// enters the request per attempt.
pub(crate) async fn recommendation(
    hook: &dyn TurnContextHook,
    chat_id: &str,
    registry: &ToolRegistry,
    content: &Value,
) -> Option<TurnNote> {
    let request = user_text(content);
    let tools = registry.definitions();
    let view = registry.toolset_view(&tools, None);
    let input = TurnContextInput {
        chat_id,
        request: &request,
        tools: &tools,
        view: &view,
    };
    let line = hook.line(&input).await.filter(|line| !line.is_empty())?;
    Some(TurnNote::new(
        TurnNoteKind::ToolsetRecommendation,
        None,
        &line,
    ))
}
