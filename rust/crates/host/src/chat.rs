use crate::*;
use agentkit_core::{ChatProvider, ChatRequest, RunOptions, ToolRegistry};
use agentkit_store::{MessageInput, SubmitTask};
use async_trait::async_trait;

#[path = "correction.rs"]
mod correction;
#[path = "chat_history.rs"]
mod history;
pub use correction::CorrectionConfig;
#[path = "chat_hooks.rs"]
mod hooks;
pub use hooks::{AttachmentBudgets, ChatExecutorOptions, ChatHookTimeouts};

/// A fixed provider binding or an immutable task snapshot determines the account.
pub struct ChatExecutor {
    provider: Arc<dyn ChatProvider>,
    tools: Arc<ToolRegistry>,
    options: ChatExecutorOptions,
}
impl ChatExecutor {
    pub fn new(provider: Arc<dyn ChatProvider>, tools: Arc<ToolRegistry>) -> Self {
        Self {
            provider,
            tools,
            options: ChatExecutorOptions::default(),
        }
    }
    pub fn with_options(mut self, options: ChatExecutorOptions) -> Self {
        self.options = options;
        self
    }
    pub fn with_provider_factory(mut self, factory: Arc<dyn ChatProviderFactory>) -> Self {
        self.options.provider_factory = Some(factory);
        self
    }
    async fn provider(&self, context: &ExecutionContext) -> HostResult<Arc<dyn ChatProvider>> {
        if let Some(factory) = &self.options.provider_factory {
            let snapshot = context
                .task
                .payload
                .get("providerSnapshot")
                .filter(|value| value.is_object())
                .ok_or_else(|| HostError::Execution("provider_snapshot_required".into()))?;
            if snapshot["model"].as_str() != Some(required(&context.task.payload, "model")?) {
                return Err(HostError::Execution(
                    "provider_snapshot_model_mismatch".into(),
                ));
            }
            return hooks::bounded(
                factory.resolve(snapshot, context.cancellation.clone()),
                context.cancellation.clone(),
                self.options.hook_timeouts.context_ms,
                "provider.resolve",
            )
            .await;
        }
        if context.task.payload["providerId"]
            .as_str()
            .is_some_and(|id| id != self.provider.id())
        {
            return Err(HostError::Execution("provider_binding_mismatch".into()));
        }
        Ok(self.provider.clone())
    }
}
pub(crate) struct CoreRuntime(pub SharedRuntime);
impl agentkit_core::Runtime for CoreRuntime {
    fn timestamp(&self) -> String {
        let timestamp = agentkit_store::ConversationTimestamp::from_epoch_millis(self.0.now_ms())
            .expect("host clock must fit JavaScript Date range");
        timestamp
            .wire_text()
            .expect("validated host clock")
            .into_owned()
    }
    fn id(&self, prefix: &str) -> String {
        format!("{prefix}{}", self.0.id())
    }
    fn now_ms(&self) -> u64 {
        self.0.now_ms().max(0) as u64
    }
}

#[async_trait]
impl TaskExecutor for ChatExecutor {
    fn handles_verification(&self) -> bool {
        self.options.verification.is_some()
    }
    fn resolves_provider_snapshots(&self) -> bool {
        self.options.provider_factory.is_some()
    }
    async fn execute(&self, context: ExecutionContext) -> HostResult<()> {
        context.check_cancelled()?;
        let chat_id = required(&context.task.payload, "chatId")?;
        let model = required(&context.task.payload, "model")?;
        if chat_id != context.task.scope_id {
            return Err(HostError::Execution("chat scope mismatch".into()));
        }
        let records = context.store.messages(chat_id, false).await?;
        let anchor = context.task.payload["assistantMessageId"]
            .as_str()
            .map(str::to_owned)
            .or_else(|| {
                records
                    .iter()
                    .find(|message| {
                        message.run_id.as_deref() == Some(&context.task.task_id)
                            && message.metadata["placeholder"] == true
                    })
                    .map(|message| message.id.clone())
            })
            .ok_or_else(|| HostError::Execution("assistant placeholder required".into()))?;
        let provider = self.provider(&context).await?;
        let continuation = history::prepare_continuation(
            &context,
            &records,
            provider.as_ref(),
            chat_id,
            model,
            &anchor,
        )
        .await?;
        let resolved = hooks::resolve_context(&self.options, &context, chat_id).await?;
        let registry = hooks::stage_registry(
            &self.options,
            &context,
            chat_id,
            self.tools.clone(),
            &resolved.bindings,
        )
        .await?;
        let messages = history::assemble(
            records,
            self.options.history_limit,
            &anchor,
            resolved.system_prompt.clone(),
        );
        let runtime: Arc<dyn agentkit_core::Runtime> =
            Arc::new(CoreRuntime(context.runtime.clone()));
        let budget = crate::restore_execution_budget(
            &context,
            self.options.execution_budgets.clone(),
            runtime.clone(),
        )
        .await?;
        let sink = Arc::new(history::DurableEvents {
            context: context.clone(),
            chat_id: chat_id.into(),
            anchor: anchor.clone(),
            provider_id: provider.id().into(),
            canonical: continuation.is_some(),
            display_metadata: context
                .store
                .get_message(&anchor)
                .await?
                .map(|message| message.metadata)
                .unwrap_or(json!({})),
            usage: self.options.usage.clone(),
            usage_timeout_ms: self.options.hook_timeouts.usage_ms,
        });
        let mut request = ChatRequest {
            model: model.into(),
            messages,
            budget: Some(budget),
            ..ChatRequest::default()
        };
        if let Some(session) = continuation {
            request.continuation = session.state;
            request.continuation_required = request.continuation.is_some();
            request.continuation_scope = Some(session.scope);
            request.continuation_store = Some(Arc::new(crate::continuation::DurableContinuation {
                context: context.clone(),
                chat_id: chat_id.into(),
                anchor: anchor.clone(),
            }));
        }
        let options = RunOptions {
            request,
            run_id: Some(context.task.task_id.clone()),
            first_seq: context.store.next_seq(&context.task.task_id).await?,
            attempt_id: Some(context.attempt_id.clone()),
            chat_id: Some(chat_id.into()),
            scope_id: Some(context.task.scope_id.clone()),
            bindings: resolved.bindings,
            max_tool_iterations: self.options.max_tool_iterations,
            max_tool_calls_per_iteration: self.options.max_tool_calls_per_iteration,
            default_tool_timeout_ms: self.options.default_tool_timeout_ms,
            cancellation: context.cancellation.clone(),
            runtime,
            event_sink: Some(sink.clone()),
            dispatch_guard: Some(context.dispatch_guard()),
            ..RunOptions::default()
        };
        let prepared = hooks::PreparedProvider {
            inner: provider,
            options: self.options.clone(),
            context: context.clone(),
            chat_id: chat_id.into(),
        };
        let session = correction::ChatSession {
            context: context.clone(),
            options: self.options.clone(),
            base: options,
            provider: Arc::new(prepared),
            registry,
            sink,
            anchor,
            system_prompt: resolved.system_prompt,
        };
        session.execute().await
    }
}
fn required<'a>(value: &'a Value, key: &str) -> HostResult<&'a str> {
    value[key]
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| HostError::Execution(format!("{key} required")))
}
pub struct SubmitChat {
    pub task_id: String,
    pub chat_id: String,
    pub model: String,
    pub content: Value,
    pub parent_message_id: Option<String>,
}
impl AgentHost {
    /// Persists the input, display placeholder, and queued work in one transaction.
    pub async fn submit_chat(&self, input: SubmitChat) -> HostResult<RunHandle> {
        if !self.supports("chat.turn") {
            return Err(HostError::UnknownKind("chat.turn".into()));
        }
        let user = format!("{}_user", input.task_id);
        let assistant = format!("{}_assistant", input.task_id);
        let task = SubmitTask {
            task_id: input.task_id.clone(),
            kind: "chat.turn".into(),
            scope_id: input.chat_id.clone(),
            payload: json!({"chatId":input.chat_id,"model":input.model,"content":input.content,"userMessageId":user,"assistantMessageId":assistant,"parentMessageId":input.parent_message_id}),
            priority: 0,
            now: self.runtime.now_ms(),
            available_at: None,
            depends_on: vec![],
            parent_task_id: None,
            exclusive_scope: true,
        };
        let messages = vec![
            MessageInput {
                id: Some(user.clone()),
                chat_id: input.chat_id.clone(),
                role: "user".into(),
                content: input.content,
                parent_message_id: input.parent_message_id,
                ..MessageInput::default()
            },
            MessageInput {
                id: Some(assistant),
                chat_id: input.chat_id,
                run_id: Some(input.task_id.clone()),
                role: "assistant".into(),
                parent_message_id: Some(user),
                metadata: json!({"placeholder":true}),
                ..MessageInput::default()
            },
        ];
        self.store.submit_turn(task, messages).await?;
        Ok(self.run_handle(&input.task_id))
    }
}

#[cfg(test)]
mod runtime_tests {
    use super::*;
    struct Clock(i64);
    impl crate::Runtime for Clock {
        fn now_ms(&self) -> i64 {
            self.0
        }
        fn id(&self) -> String {
            "test".into()
        }
    }
    #[test]
    fn core_runtime_formats_entire_javascript_date_range_without_chrono_panics() {
        for (epoch, expected) in [
            (-8_640_000_000_000_000, "-271821-04-20T00:00:00.000Z"),
            (8_640_000_000_000_000, "+275760-09-13T00:00:00.000Z"),
            (253_402_300_800_000, "+010000-01-01T00:00:00.000Z"),
            (-1, "1969-12-31T23:59:59.999Z"),
        ] {
            let runtime = CoreRuntime(Arc::new(Clock(epoch)));
            assert_eq!(agentkit_core::Runtime::timestamp(&runtime), expected);
        }
    }
    #[test]
    #[should_panic(expected = "host clock must fit JavaScript Date range")]
    fn core_runtime_rejects_clock_outside_explicit_precondition() {
        let runtime = CoreRuntime(Arc::new(Clock(8_640_000_000_000_001)));
        agentkit_core::Runtime::timestamp(&runtime);
    }
}
