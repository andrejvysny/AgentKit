use super::{
    history::DurableEvents,
    hooks::{ChatExecutorOptions, bounded},
};
use crate::*;
use agentkit_core::{ChatMessage, ChatProvider, RunOptions, Terminal, ToolRegistry};
use agentkit_store::MessageInput;
use futures::StreamExt;
use std::collections::HashSet;
#[path = "correction_messages.rs"]
mod messages;
use messages::*;
#[derive(Clone)]
pub struct CorrectionConfig {
    pub max_passes: u32,
    pub include_user_request: bool,
}
impl Default for CorrectionConfig {
    fn default() -> Self {
        Self {
            max_passes: 3,
            include_user_request: true,
        }
    }
}
pub(super) struct ChatSession {
    pub context: ExecutionContext,
    pub options: ChatExecutorOptions,
    pub base: RunOptions,
    pub provider: Arc<dyn ChatProvider>,
    pub registry: Arc<ToolRegistry>,
    pub sink: Arc<DurableEvents>,
    pub anchor: String,
    pub system_prompt: Option<String>,
}
struct Pass {
    terminal: Terminal,
    content: String,
    tool_calls: usize,
    failure: Option<ExecutionFailure>,
    messages: Vec<ChatMessage>,
}
impl ChatSession {
    pub async fn execute(mut self) -> HostResult<()> {
        match self.drive().await {
            Ok(pass) => {
                if self.context.suspension.lock().await.is_some() {
                    // A parked call is settled by the park itself: the
                    // anchor stays a placeholder (the answer finalizes it),
                    // and the attempt and lease already ended. Finalizing
                    // here would fight the park with a dead fence.
                    return Ok(());
                }
                if self.context.interruption.is_cancelled() {
                    // The loop stopped at a safe boundary on a durable
                    // interrupt: no final answer is owed, the anchor stays
                    // a placeholder, and the runner parks this attempt. If
                    // the final answer already committed (the interrupt
                    // raced the finish), keep finalizing instead.
                    let landed = self
                        .context
                        .store
                        .events(&self.context.task.task_id, None, 1000)
                        .await
                        .map_err(|error| HostError::Execution(error.to_string()))?
                        .iter()
                        .any(|event| {
                            event.event["type"] == "run.completed"
                                && event.event["attemptId"]
                                    == Value::String(self.context.attempt_id.clone())
                        });
                    if !landed {
                        return Ok(());
                    }
                }
                self.finish(&pass).await?;
                match pass.terminal {
                    Terminal::Completed => Ok(()),
                    Terminal::Cancelled => Err(HostError::Cancelled),
                    Terminal::Failed => Err(pass
                        .failure
                        .unwrap_or_else(|| ExecutionFailure::new("provider run failed"))
                        .into()),
                }
            }
            Err(error) => {
                if matches!(error, HostError::Cancelled) {
                    self.context
                        .emit("run.cancelled", json!({"reason":"aborted"}))
                        .await?;
                } else {
                    let message = error.to_string();
                    let code = message.split(':').next().unwrap_or("host_error");
                    self.context
                        .emit(
                            "run.failed",
                            json!({"errorCode":code,"errorMessage":message}),
                        )
                        .await?;
                }
                Err(error)
            }
        }
    }
    async fn drive(&mut self) -> HostResult<Pass> {
        let initial = self.base.request.messages.clone();
        let mut pass = self
            .run_pass(
                initial.clone(),
                self.registry.clone(),
                self.options.max_tool_iterations,
            )
            .await?;
        let mut passes = 1;
        if self.can_retry_chat_only(&pass) {
            passes += 1;
            self.boundary(
                passes,
                "chat_only",
                "The provider rejected the request with tools attached; re-asking without them.",
            )
            .await?;
            let messages = filter_tools(pass.messages.clone());
            pass = self
                .run_pass(messages, Arc::new(ToolRegistry::new()), 1)
                .await?;
        } else if pass.terminal == Terminal::Completed
            && pass.content.trim().is_empty()
            && pass.tool_calls == 0
        {
            passes += 1;
            self.boundary(passes,"empty_response","The model completed the turn without an answer; asking the original question once more.").await?;
            let messages = if self.private() {
                self.followup(
                    &pass,
                    "Please provide an answer to the previous request.",
                    None,
                )
                .await?
            } else {
                filter_tools(initial.clone())
            };
            pass = self
                .run_pass(messages, Arc::new(ToolRegistry::new()), 1)
                .await?;
        }
        if pass.terminal != Terminal::Cancelled
            && pass.content.trim().is_empty()
            && pass.tool_calls == 0
        {
            self.context
                .emit(
                    "run.warning",
                    json!({"code":"empty_response","message":"The model returned no answer."}),
                )
                .await?;
        }
        let staged = self
            .registry
            .definitions()
            .into_iter()
            .map(|tool| tool.name)
            .collect::<HashSet<_>>();
        if !staged.is_empty()
            && pass.tool_calls == 0
            && looks_like_emulated_tool_call(&pass.content, &staged)
        {
            self.context
                .emit(
                    "run.warning",
                    json!({"code":"emulated_tool_call","message":EMULATED_TOOL_CALL_MESSAGE}),
                )
                .await?;
            self.context
                .store
                .append_message(
                    MessageInput {
                        chat_id: self.sink.chat_id.clone(),
                        run_id: Some(self.context.task.task_id.clone()),
                        role: "system".into(),
                        content: EMULATED_TOOL_CALL_MESSAGE.into(),
                        metadata: json!({"banner":"emulated_tool_call"}),
                        ..Default::default()
                    },
                    Some(self.context.fence.clone()),
                    self.context.runtime.now_ms(),
                )
                .await?;
        }
        if pass.terminal == Terminal::Completed
            && pass.tool_calls > 0
            && self.options.verification.is_some()
        {
            pass = self.verify(pass, passes, &initial).await?;
        }
        Ok(pass)
    }
    async fn run_pass(
        &mut self,
        messages: Vec<ChatMessage>,
        registry: Arc<ToolRegistry>,
        iterations: u32,
    ) -> HostResult<Pass> {
        self.context.check_cancelled()?;
        let mut options = self.base.clone();
        options.request.messages = messages.clone();
        options.first_seq = self
            .context
            .store
            .next_seq(&self.context.task.task_id)
            .await?;
        options.max_tool_iterations = iterations;
        let mut handle = agentkit_core::run_chat(options, self.provider.clone(), registry);
        let mut content = String::new();
        let mut calls = HashSet::new();
        let mut failure = None;
        while let Some(event) = handle.events.next().await {
            let event = serde_json::to_value(event)
                .map_err(|error| HostError::Execution(error.to_string()))?;
            match event["type"].as_str() {
                Some("run.message.delta") => {
                    content.push_str(event["data"]["delta"].as_str().unwrap_or(""))
                }
                Some("run.message.completed") => {
                    content = event["data"]["content"].as_str().unwrap_or("").into()
                }
                Some("run.tool.requested") => {
                    if let Some(id) = event["data"]["toolCallId"].as_str() {
                        calls.insert(id.to_owned());
                    }
                }
                Some("run.failed") => {
                    failure = Some(ExecutionFailure {
                        code: event["data"]["errorCode"].as_str().map(str::to_owned),
                        ..ExecutionFailure::new(
                            event["data"]["errorMessage"]
                                .as_str()
                                .unwrap_or("provider run failed"),
                        )
                    });
                }
                _ => {}
            }
        }
        let result = handle
            .result
            .await
            .map_err(|error| HostError::Execution(error.to_string()))?;
        if let Some(state) = result.continuation {
            self.base.request.continuation = Some(state);
            self.base.request.continuation_required = true;
        }
        let mut transcript = messages;
        transcript.extend(result.appended_messages);
        Ok(Pass {
            terminal: result.terminal,
            content,
            tool_calls: calls.len(),
            failure,
            messages: transcript,
        })
    }
    fn can_retry_chat_only(&self, pass: &Pass) -> bool {
        pass.terminal == Terminal::Failed
            && !self.registry.is_empty()
            && pass.tool_calls == 0
            && !self.context.cancellation.is_cancelled()
            && !self.private()
            && matches!(
                pass.failure
                    .as_ref()
                    .and_then(|failure| failure.code.as_deref()),
                Some("400" | "422" | "unsupported_tools" | "invalid_tool_schema")
            )
    }
    fn private(&self) -> bool {
        self.base.request.continuation_scope.is_some()
    }
    async fn boundary(&self, pass: u32, reason: &str, message: &str) -> HostResult<()> {
        if let Some(budget) = &self.base.request.budget {
            budget
                .consume(if reason == "correction" {
                    "correctionPasses"
                } else {
                    "retries"
                })
                .await
                .map_err(|error| HostError::Execution(error.to_string()))?;
        }
        self.context
            .emit(
                "run.warning",
                json!({"code":"retry_pass","message":message,"pass":pass,"reason":reason}),
            )
            .await?;
        self.context
            .store
            .update_message(
                &self.anchor,
                Some("".into()),
                None,
                Some(self.context.fence.clone()),
                self.context.runtime.now_ms(),
            )
            .await?;
        Ok(())
    }
    async fn followup(
        &self,
        pass: &Pass,
        text: &str,
        correction: Option<u32>,
    ) -> HostResult<Vec<ChatMessage>> {
        let mut messages = pass.messages.clone();
        messages.push(ChatMessage::text("user", text));
        let mut metadata = json!({"internal":true});
        if self.private() {
            metadata["canonicalProviderTurn"] = true.into();
        }
        if let Some(pass) = correction {
            metadata["correctionPass"] = pass.into();
        }
        self.context
            .store
            .append_message(
                MessageInput {
                    chat_id: self.sink.chat_id.clone(),
                    run_id: Some(self.context.task.task_id.clone()),
                    role: "user".into(),
                    content: text.into(),
                    metadata,
                    ..Default::default()
                },
                Some(self.context.fence.clone()),
                self.context.runtime.now_ms(),
            )
            .await?;
        Ok(messages)
    }
    async fn verify(
        &mut self,
        mut pass: Pass,
        passes: u32,
        initial: &[ChatMessage],
    ) -> HostResult<Pass> {
        let Some(verification) = self.options.verification.clone() else {
            return Ok(pass);
        };
        let correction = self.options.correction.clone();
        let cap = correction
            .as_ref()
            .map_or(0, |config| config.max_passes.min(5));
        let mut previous = None;
        let mut index = 0;
        let mut last_report = None;
        loop {
            self.context.check_cancelled()?;
            let input = VerificationInput {
                context: self.context.clone(),
                final_content: pass.content.clone(),
                tool_call_count: pass.tool_calls,
                pass: index,
            };
            let report = bounded(
                verification.verify_optional(input),
                self.context.cancellation.clone(),
                self.options.hook_timeouts.verify_ms,
                "verify",
            )
            .await;
            self.context.check_cancelled()?;
            let report = match report {
                Ok(Some(report)) if ["pass", "partial"].contains(&report.status.as_str()) => report,
                Ok(None) => {
                    if correction.is_some() {
                        self.verification_event(index, "unavailable", &[]).await?;
                    }
                    break;
                }
                Ok(_) => return Err(HostError::Execution("invalid verification status".into())),
                Err(error) if correction.is_none() => return Err(error),
                Err(_) => {
                    self.verification_event(index, "unavailable", &[]).await?;
                    break;
                }
            };
            if correction.is_some() {
                self.verification_event(index, &report.status, &report.deficiencies)
                    .await?;
            }
            let continue_pass = report.status == "partial"
                && !report.deficiencies.is_empty()
                && index < cap
                && previous.is_none_or(|count| report.deficiencies.len() < count);
            last_report = Some(report.clone());
            if pass.terminal != Terminal::Completed || !continue_pass {
                break;
            }
            previous = Some(report.deficiencies.len());
            index += 1;
            self.boundary(passes+index,"correction",&format!("Verification found {} unresolved item(s); correcting them (pass {index} of {cap}).",report.deficiencies.len())).await?;
            let write_back = write_back(&report.deficiencies);
            let mut messages = self.followup(&pass, &write_back, Some(index)).await?;
            if !self.private() {
                messages = correction_messages(
                    self.system_prompt.as_deref(),
                    if correction
                        .as_ref()
                        .is_some_and(|config| config.include_user_request)
                    {
                        last_user(initial)
                    } else {
                        None
                    },
                    &pass.content,
                    &write_back,
                );
            }
            let superseded = pass.content.clone();
            pass = self
                .run_pass(
                    messages,
                    self.registry.clone(),
                    self.options.max_tool_iterations,
                )
                .await?;
            if pass.content.trim().is_empty() {
                pass.content = superseded;
            }
            if pass.terminal != Terminal::Completed {
                break;
            }
        }
        if let Some(report) = last_report.filter(|report| report.status != "pass") {
            self.banner(&report).await?;
        }
        Ok(pass)
    }
    async fn verification_event(
        &self,
        pass: u32,
        status: &str,
        deficiencies: &[String],
    ) -> HostResult<()> {
        self.context
            .emit(
                "run.verification",
                json!({"pass":pass,"status":status,"deficiencies":deficiencies}),
            )
            .await
    }
    async fn banner(&self, report: &VerificationReport) -> HostResult<()> {
        let text = if report.deficiencies.is_empty() {
            "Verification did not pass, but reported no specific deficiency.".into()
        } else {
            format!(
                "Verification found {} unresolved item(s):\n{}",
                report.deficiencies.len(),
                report
                    .deficiencies
                    .iter()
                    .map(|line| format!("- {line}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        };
        self.context
            .store
            .append_message(
                MessageInput {
                    chat_id: self.sink.chat_id.clone(),
                    run_id: Some(self.context.task.task_id.clone()),
                    role: "system".into(),
                    content: text.into(),
                    metadata: json!({"banner":"verification","status":report.status}),
                    ..Default::default()
                },
                Some(self.context.fence.clone()),
                self.context.runtime.now_ms(),
            )
            .await?;
        Ok(())
    }
    async fn finish(&self, pass: &Pass) -> HostResult<()> {
        let mut metadata = self
            .context
            .store
            .get_message(&self.anchor)
            .await?
            .map(|message| message.metadata)
            .unwrap_or(json!({}));
        metadata["placeholder"] = false.into();
        if self.private() {
            metadata["canonicalProviderDisplay"] = true.into();
        }
        self.context
            .store
            .update_message(
                &self.anchor,
                Some(pass.content.clone().into()),
                Some(metadata),
                Some(self.context.fence.clone()),
                self.context.runtime.now_ms(),
            )
            .await?;
        Ok(())
    }
}
