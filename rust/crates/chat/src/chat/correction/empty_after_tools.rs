//! A turn that used tools and then ended with an empty reply is asked once
//! more to continue, with its tools and the steps it has left.
use super::super::hooks::message_text;
use super::{ChatResult, ChatSession, Pass};
use agentkit_core::{ChatMessage, Terminal};

const NUDGE: &str = "Your last reply was empty. Continue the task from where you stopped: make the next tool call, or give your final answer. Do not repeat steps that already succeeded.";

impl ChatSession {
    /// Only a reply the model ended itself (`stop`): `repeated_failure` and
    /// `max_iterations` ends are deliberate and a re-ask would undo them; a
    /// `length` cut would only repeat.
    pub(super) fn reask_after_tools_due(&self, pass: &Pass) -> bool {
        pass.terminal == Terminal::Completed
            && !pass.suspended
            && pass.tool_calls > 0
            && pass.final_empty
            && pass.finish_reason.as_deref() == Some("stop")
            && !self.context.cancellation.is_cancelled()
            && !self.context.interruption.is_cancelled()
    }

    pub(super) async fn reask_after_tools(&mut self, pass: Pass, passes: u32) -> ChatResult<Pass> {
        self.boundary(
            passes,
            "empty_after_tools",
            "The model ended the turn without an answer after using tools; asking it to continue.",
        )
        .await?;
        let messages = self.followup(&pass, NUDGE, None).await?;
        // The steps left, never a fresh budget: agent tasks run under a hard
        // request cap.
        let steps = self
            .options
            .max_tool_iterations
            .saturating_sub(pass.iterations)
            .max(1);
        // The nudge is a user message: without the turn's index the
        // quoted-call guard would start the turn at it and stop seeing the
        // tool output read before it.
        let turn_start = turn_user(&self.base.request.messages);
        // The empty reply committed already; only the re-ask's own terminal
        // may finalize the turn once a durable interrupt parks it.
        self.answer_committed = false;
        let registry = self.registry.clone();
        let mut next = self
            .run_pass_from(messages, registry, steps, turn_start)
            .await?;
        // The turn used tools, whatever the re-ask did: verification and the
        // final notices judge the turn, as they would one longer pass.
        next.tool_calls += pass.tool_calls;
        Ok(next)
    }
}

/// The turn used tools and its final reply, on an end the model chose, is
/// empty.
pub(super) fn unanswered_after_tools(pass: &Pass) -> bool {
    pass.terminal == Terminal::Completed
        && pass.tool_calls > 0
        && pass.final_empty
        && matches!(pass.finish_reason.as_deref(), Some("stop" | "length"))
}

pub(super) fn reply_is_empty(appended: &[ChatMessage]) -> bool {
    appended.last().is_some_and(|message| {
        message.role == "assistant"
            && message.tool_calls.is_empty()
            && message_text(&message.content).trim().is_empty()
    })
}

/// The rule the quoted-call guard applies to the original request: its last
/// typed user message.
fn turn_user(messages: &[ChatMessage]) -> Option<usize> {
    messages
        .iter()
        .rposition(|message| message.role == "user" && message.tool_call_id.is_none())
}
