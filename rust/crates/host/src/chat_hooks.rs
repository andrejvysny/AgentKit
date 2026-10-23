use crate::*;
use agentkit_core::{
    BudgetLimits, ChatMessage, ChatProvider, ChatRequest, CoreError, ProviderEvent, ProviderStream,
};
use async_trait::async_trait;
use futures::{StreamExt, TryStreamExt};
use serde::Deserialize;
use std::{collections::HashMap, future::Future, time::Duration};

#[derive(Clone)]
pub struct ChatExecutorOptions {
    pub context: Option<Arc<dyn ContextProvider>>,
    pub attachments: Option<Arc<dyn AttachmentResolver>>,
    pub usage: Option<Arc<dyn UsageAuthorizer>>,
    pub provider_factory: Option<Arc<dyn ChatProviderFactory>>,
    pub verification: Option<Arc<dyn VerificationHook>>,
    pub correction: Option<super::correction::CorrectionConfig>,
    pub tool_contributors: Vec<Arc<dyn ToolContributor>>,
    pub tool_guards: Vec<Arc<dyn ToolGuard>>,
    pub actor: Value,
    pub actor_id: Option<String>,
    pub hook_timeouts: ChatHookTimeouts,
    pub attachment_budgets: AttachmentBudgets,
    pub history_limit: usize,
    pub execution_budgets: BudgetLimits,
    pub max_tool_iterations: u32,
    pub max_tool_calls_per_iteration: usize,
    pub default_tool_timeout_ms: Option<u64>,
}
impl Default for ChatExecutorOptions {
    fn default() -> Self {
        Self {
            context: None,
            attachments: None,
            usage: None,
            provider_factory: None,
            verification: None,
            correction: None,
            tool_contributors: vec![],
            tool_guards: vec![],
            actor: Value::Null,
            actor_id: None,
            hook_timeouts: ChatHookTimeouts::default(),
            attachment_budgets: AttachmentBudgets::default(),
            history_limit: 200,
            execution_budgets: BudgetLimits::default(),
            max_tool_iterations: 4,
            max_tool_calls_per_iteration: 8,
            default_tool_timeout_ms: None,
        }
    }
}
#[derive(Clone)]
pub struct ChatHookTimeouts {
    pub context_ms: u64,
    pub attachments_ms: u64,
    pub usage_ms: u64,
    pub verify_ms: u64,
    pub contribute_ms: u64,
}
impl Default for ChatHookTimeouts {
    fn default() -> Self {
        Self {
            context_ms: 10_000,
            attachments_ms: 10_000,
            usage_ms: 10_000,
            verify_ms: 30_000,
            contribute_ms: 15_000,
        }
    }
}
#[derive(Clone)]
pub struct AttachmentBudgets {
    pub max_bytes_per_image: usize,
    pub max_total_bytes: usize,
    pub max_images: usize,
}
impl Default for AttachmentBudgets {
    fn default() -> Self {
        Self {
            max_bytes_per_image: 5 * 1024 * 1024,
            max_total_bytes: 20 * 1024 * 1024,
            max_images: 16,
        }
    }
}
#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ContextSnapshot {
    #[serde(
        default,
        deserialize_with = "agentkit_contracts::deserialize_preserving"
    )]
    pub bindings: Vec<Value>,
    pub system_prompt: Option<String>,
}

pub(crate) async fn resolve_context(
    options: &ChatExecutorOptions,
    context: &ExecutionContext,
    chat_id: &str,
) -> HostResult<ContextSnapshot> {
    let Some(provider) = &options.context else {
        return Ok(ContextSnapshot::default());
    };
    let input =
        json!({"chatId":chat_id,"runId":context.task.task_id,"scopeId":context.task.scope_id});
    let resolved = bounded(
        provider.resolve(input, context.cancellation.clone()),
        context.cancellation.clone(),
        options.hook_timeouts.context_ms,
        "context",
    )
    .await;
    match resolved {
        Ok(value) => {
            let snapshot: ContextSnapshot = serde_json::from_value(value)
                .map_err(|_| HostError::Execution("invalid context provider result".into()))?;
            for binding in &snapshot.bindings {
                agentkit_contracts::validate_schema("AiContextBindingSchema", binding)
                    .map_err(|_| HostError::Execution("invalid context binding".into()))?;
            }
            Ok(snapshot)
        }
        Err(HostError::Execution(error)) if error.starts_with("hook_timeout:") => {
            context.emit("run.warning",json!({"code":"hook_timeout","message":"The context provider did not answer in time; this turn ran with no context bindings or system prompt."})).await?;
            Ok(ContextSnapshot::default())
        }
        Err(error) => Err(error),
    }
}
pub(crate) async fn bounded<T>(
    future: impl Future<Output = HostResult<T>>,
    cancellation: CancellationToken,
    milliseconds: u64,
    hook: &str,
) -> HostResult<T> {
    tokio::select! {biased;
        _=cancellation.cancelled()=>Err(HostError::Cancelled),
        value=async {if milliseconds==0 {future.await} else {tokio::time::timeout(Duration::from_millis(milliseconds),future).await.map_err(|_|HostError::Execution(format!("hook_timeout: {hook}")))?}}=>value,
    }
}
#[derive(Clone)]
pub(crate) struct PreparedProvider {
    pub inner: Arc<dyn ChatProvider>,
    pub options: ChatExecutorOptions,
    pub context: ExecutionContext,
    pub chat_id: String,
}
#[async_trait]
impl ChatProvider for PreparedProvider {
    fn supports_usage_cleanup(&self) -> bool {
        self.inner.supports_usage_cleanup()
    }
    fn id(&self) -> &str {
        self.inner.id()
    }
    fn connection_identity(&self) -> Option<agentkit_core::providers::ConnectionIdentity> {
        self.inner.connection_identity()
    }
    fn tracks_transport_requests(&self) -> bool {
        self.inner.tracks_transport_requests()
    }
    fn stream_chat(
        &self,
        mut input: ChatRequest,
        cancellation: CancellationToken,
    ) -> ProviderStream {
        let provider = self.clone();
        Box::pin(futures::stream::once(async move {
            let (messages,warnings)=resolve_attachments(&provider.options,&provider.context,&provider.chat_id,input.messages).await.map_err(core_error)?;
            input.messages=messages;
            if let Some(usage)=&provider.options.usage {
                let estimate=input.messages.iter().map(|message|message_text(&message.content).encode_utf16().count()).sum::<usize>().div_ceil(4);
                let request=json!({"runId":input.run_id,"chatId":provider.chat_id,"providerId":provider.inner.id(),"model":input.model,"estimatedPromptTokens":estimate});
                bounded(usage.authorize(&input.run_id,&request),cancellation.clone(),provider.options.hook_timeouts.usage_ms,"usage.authorize").await.map_err(core_error)?;
            }
            if cancellation.is_cancelled() {return Err(CoreError::new("cancelled","aborted"));}
            let warnings=futures::stream::iter(warnings.into_iter().map(Ok));
            Ok(warnings.chain(provider.inner.stream_chat(input,cancellation)))
        }).try_flatten())
    }
}
fn core_error(error: HostError) -> CoreError {
    if matches!(error, HostError::Cancelled) {
        CoreError::new("cancelled", "aborted")
    } else {
        CoreError::new("host_hook_failed", &error.to_string())
    }
}
pub(crate) fn message_text(value: &Value) -> String {
    if let Some(text) = value.as_str() {
        return text.into();
    }
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|part| part["text"].as_str().filter(|_| part["type"] == "text"))
        .collect::<Vec<_>>()
        .join("\n")
}
async fn resolve_attachments(
    options: &ChatExecutorOptions,
    context: &ExecutionContext,
    chat_id: &str,
    mut messages: Vec<ChatMessage>,
) -> HostResult<(Vec<ChatMessage>, Vec<ProviderEvent>)> {
    let mut cache = HashMap::<String, Option<Value>>::new();
    let mut total = 0;
    let mut images = 0;
    let mut warnings = vec![];
    for message in &mut messages {
        let Some(input) = message.content.as_array() else {
            continue;
        };
        let mut parts = vec![];
        for part in input {
            if part["type"] != "image" || part["source"]["kind"] != "ref" {
                parts.push(part.clone());
                continue;
            }
            let reference = part["source"]["ref"]
                .as_str()
                .ok_or_else(|| HostError::Execution("invalid attachment reference".into()))?;
            if !cache.contains_key(reference) {
                let resolved = resolve_one(options, context, chat_id, reference).await?;
                cache.insert(reference.into(), resolved);
            }
            let Some(source) = cache[reference].clone() else {
                let reason = if options.attachments.is_none() {
                    "no AttachmentResolver is wired."
                } else {
                    "the resolver has no bytes for it."
                };
                warnings.push(warning(
                    "attachment_unresolved",
                    &format!("Attachment \"{reference}\" was dropped from this pass: {reason}"),
                ));
                continue;
            };
            let bytes = source["base64"]
                .as_str()
                .map(str::len)
                .unwrap_or(0)
                .saturating_mul(3)
                / 4;
            if let Some(reason) = refusal(&options.attachment_budgets, bytes, total, images) {
                warnings.push(warning(
                    "attachment_budget_exceeded",
                    &format!("Attachment \"{reference}\" was dropped from this pass: {reason}"),
                ));
                continue;
            }
            total += bytes;
            images += 1;
            let mut resolved = part.clone();
            resolved["source"] = source;
            parts.push(resolved);
        }
        message.content = if parts.is_empty() {
            Value::String(String::new())
        } else {
            parts.into()
        };
    }
    Ok((messages, warnings))
}
async fn resolve_one(
    options: &ChatExecutorOptions,
    context: &ExecutionContext,
    chat_id: &str,
    reference: &str,
) -> HostResult<Option<Value>> {
    let Some(resolver) = &options.attachments else {
        return Ok(None);
    };
    let scope = AttachmentResolveContext {
        chat_id: chat_id.into(),
    };
    let result = bounded(
        resolver.resolve(reference, &scope, context.cancellation.clone()),
        context.cancellation.clone(),
        options.hook_timeouts.attachments_ms,
        "attachments.resolve",
    )
    .await;
    let result = match result {
        Err(HostError::Execution(error)) if error.starts_with("hook_timeout:") => None,
        other => other?,
    };
    result
        .map(|resolved| {
            let value =
                json!({"kind":"data","mediaType":resolved.media_type,"base64":resolved.base64});
            agentkit_contracts::validate_schema("AiImageSourceSchema", &value)
                .map_err(|_| HostError::Execution("invalid resolved attachment".into()))?;
            Ok(value)
        })
        .transpose()
}
fn warning(code: &str, message: &str) -> ProviderEvent {
    ProviderEvent::Warning {
        code: code.into(),
        message: message.into(),
    }
}
fn refusal(
    budget: &AttachmentBudgets,
    bytes: usize,
    total: usize,
    images: usize,
) -> Option<String> {
    if bytes > budget.max_bytes_per_image {
        Some(format!(
            "its {bytes} decoded bytes exceed the {}-byte per-image budget.",
            budget.max_bytes_per_image
        ))
    } else if total.saturating_add(bytes) > budget.max_total_bytes {
        Some(format!(
            "its {bytes} decoded bytes would push this pass past the {}-byte total budget ({total} already used).",
            budget.max_total_bytes
        ))
    } else if images >= budget.max_images {
        Some(format!(
            "this pass already carries its budgeted {} image(s).",
            budget.max_images
        ))
    } else {
        None
    }
}

pub(super) async fn stage_registry(
    options: &ChatExecutorOptions,
    context: &ExecutionContext,
    chat_id: &str,
    static_registry: Arc<agentkit_core::ToolRegistry>,
    bindings: &[Value],
) -> HostResult<Arc<agentkit_core::ToolRegistry>> {
    if options.tool_contributors.is_empty() && options.tool_guards.is_empty() {
        return Ok(static_registry);
    }
    let reads = Arc::new(StaticToolContributor::new(static_registry));
    for name in reads.skipped_write_names() {
        context.emit("run.warning",json!({"code":"tool_skipped","message":format!("Tool {name} was omitted: static write tools require a governed contributor.")})).await?;
    }
    let mut contributors = options.tool_contributors.clone();
    contributors.insert(0, reads);
    let staged = stage_tools(
        &contributors,
        &options.tool_guards,
        ToolContributionContext {
            chat_id: Some(chat_id.into()),
            run_id: context.task.task_id.clone(),
            scope_id: Some(context.task.scope_id.clone()),
            actor: options.actor.clone(),
            actor_id: options.actor_id.clone(),
            bindings: bindings.to_vec(),
            limits: agentkit_core::ToolLimits::default(),
            cancellation: context.cancellation.clone(),
            has_primary_binding: bindings
                .iter()
                .any(|binding| binding["role"] == "primary" && binding["status"] == "active"),
            timeout_ms: options.hook_timeouts.contribute_ms,
        },
    )
    .await?;
    context.check_cancelled()?;
    for failure in staged.failed {
        context.emit("run.warning",json!({"code":if failure.timed_out {"hook_timeout"} else {"tool_contributor_failed"},"message":format!("Tool contributor {} failed: {}.",failure.namespace,failure.reason)})).await?;
    }
    for name in staged.skipped {
        context.emit("run.warning",json!({"code":"tool_schema_invalid","message":format!("Tool {name} was omitted: invalid definition or schema.")})).await?;
    }
    Ok(staged.registry)
}
