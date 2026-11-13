use super::super::hooks::message_text;
use agentkit_core::ChatMessage;

pub(super) fn filter_tools(messages: Vec<ChatMessage>) -> Vec<ChatMessage> {
    messages
        .into_iter()
        .filter(|message| message.role != "tool" && message.tool_calls.is_empty())
        .collect()
}
pub(super) fn last_user(messages: &[ChatMessage]) -> Option<String> {
    messages
        .iter()
        .rev()
        .find(|message| message.role == "user")
        .map(|message| message_text(&message.content))
        .filter(|text| !text.trim().is_empty())
}
pub(super) fn write_back(deficiencies: &[String]) -> String {
    format!(
        "Your last attempt was verified and did not fully land. {} item(s) are still unresolved:\n{}\n\nFix each of these now by calling your tools — do not answer with a plan or a description of what should be done. Leave the parts that already succeeded alone, and when you are finished say briefly what you changed.",
        deficiencies.len(),
        deficiencies
            .iter()
            .map(|line| format!("- {line}"))
            .collect::<Vec<_>>()
            .join("\n")
    )
}
pub(super) fn correction_messages(
    system: Option<&str>,
    user: Option<String>,
    previous: &str,
    write_back: &str,
) -> Vec<ChatMessage> {
    let mut messages = vec![];
    if let Some(system) = system.filter(|text| !text.is_empty()) {
        messages.push(ChatMessage::text("system", system));
    }
    if let Some(user) = user {
        messages.push(ChatMessage::text("user", &user));
    }
    if !previous.trim().is_empty() {
        messages.push(ChatMessage::text("assistant", previous));
    }
    messages.push(ChatMessage::text("user", write_back));
    messages
}
