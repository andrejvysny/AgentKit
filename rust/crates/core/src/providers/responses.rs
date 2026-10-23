use super::{
    chat_completions::usage_data,
    responses_request::{build_responses_request, make_continuation},
    responses_state::{ResponsesResult, ResponsesState},
    transport::{dispatch, read_json},
    AuthKind, ConnectionIdentity, TrustedTransport,
};
use crate::{sse::SseDecoder, ChatProvider, ChatRequest, CoreError, ProviderEvent, ProviderStream};
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
#[derive(Clone)]
pub struct ResponsesProvider {
    transport: Arc<dyn TrustedTransport>,
    identity: ConnectionIdentity,
}
impl ResponsesProvider {
    pub fn new(transport: Arc<dyn TrustedTransport>) -> Result<Self, CoreError> {
        let identity = transport.identity();
        if identity.provider_id.is_empty()
            || identity.connection_id.is_empty()
            || identity.protocol != "responses"
        {
            return Err(CoreError::responses("invalid_connection"));
        }
        Ok(Self {
            transport,
            identity,
        })
    }
    fn events(&self, input: ChatRequest, cancellation: CancellationToken) -> ProviderStream {
        let provider = self.clone();
        Box::pin(async_stream::try_stream! {
            yield ProviderEvent::Started;
            if cancellation.is_cancelled() {Err(CoreError::new("cancelled","aborted"))?;}
            let body=build_responses_request(&input,&provider.identity,provider.transport.auth_kind()==AuthKind::ChatGptAccount)?;
            if let Some(budget)=&input.budget {budget.consume("providerRequests").await?;}
            let response=dispatch(provider.transport.as_ref(),&provider.identity,"responses",Some(body.clone()),cancellation.clone()).await?;
            provider.transport.check_current()?;
            let mut bytes=response.bytes_stream();let mut decoder=SseDecoder::new();let mut state=ResponsesState::default();
            let mut emitted=String::new();let call_id=input.runtime.as_ref().map(|runtime|runtime.id("call_")).unwrap_or_else(||format!("call_{}",uuid::Uuid::new_v4()));let mut failure=None;
            loop {
                let next=tokio::select! {biased;
                    _=cancellation.cancelled()=>{failure=Some(CoreError::new("cancelled","aborted"));None},
                    next=bytes.next()=>next,
                };
                provider.transport.check_current()?;
                let Some(chunk)=next else {break;};
                let frames=match chunk {Ok(chunk)=>decoder.push(&chunk),Err(_)=>Err(CoreError::responses("transport_error"))};
                match frames {
                    Ok(frames)=>for frame in frames {
                        if let Err(error)=provider.transport.check_current() {failure=Some(error);break;}
                        if frame.done {continue;}
                        let value=agentkit_contracts::parse_javascript_json(&frame.data).map_err(|_|CoreError::responses("malformed_response"));
                        match value.and_then(|value|state.accept(value)) {
                            Ok(delta)=>{yield ProviderEvent::Activity;if let Some(delta)=delta {emitted.push_str(&delta);yield ProviderEvent::Delta(delta);}}
                            Err(error)=>{failure=Some(error);break;}
                        }
                    },
                    Err(error)=>failure=Some(error),
                }
                if failure.is_some() {break;}
            }
            if failure.is_none() {match decoder.finish() {
                Ok(frames)=>for frame in frames {if !frame.done {
                    if let Err(error)=provider.transport.check_current() {failure=Some(error);break;}
                    let value=agentkit_contracts::parse_javascript_json(&frame.data).map_err(|_|CoreError::responses("malformed_response"));
                    match value.and_then(|value|state.accept(value)) {
                        Ok(delta)=>{yield ProviderEvent::Activity;if let Some(delta)=delta {emitted.push_str(&delta);yield ProviderEvent::Delta(delta);}}
                        Err(error)=>{failure=Some(error);break;}
                    }
                }},Err(error)=>failure=Some(error),
            }}
            let observed_usage=state.usage.clone();
            let outcome=if let Some(error)=failure {Err(error)} else {state.finish(&input.tools)};
            let result=match outcome {
                Ok(result)=>result,
                Err(error)=>{if let Some(usage)=observed_usage {if let Ok(data)=response_usage(&usage,&input.model,&call_id,false) {yield ProviderEvent::Usage(data);}}Err(error)?;unreachable!()}
            };
            if let Some(usage)=&result.usage {response_usage(usage,&input.model,&call_id,true)?;}
            let persisted=provider.persist(&input,&body,&result,cancellation.clone()).await;
            match persisted {
                Ok(Some(state))=>yield ProviderEvent::Continuation(state),
                Ok(None)=>{},
                Err(error)=>{
                    if let Some(usage)=&result.usage {if let Ok(data)=response_usage(usage,&input.model,&call_id,false) {yield ProviderEvent::Usage(data);}}
                    Err(error)?;
                }
            }
            if result.content.len()>emitted.len() {yield ProviderEvent::Delta(result.content[emitted.len()..].into());}
            if let Some(usage)=&result.usage {yield ProviderEvent::Usage(response_usage(usage,&input.model,&call_id,true)?);}
            for call in &result.calls {yield ProviderEvent::ToolRequested(call.clone());}
            yield ProviderEvent::Completed {content:result.content,finish_reason:if result.calls.is_empty() {"stop".into()} else {"tool_calls".into()},tool_calls:result.calls};
        })
    }
    async fn persist(
        &self,
        input: &ChatRequest,
        body: &Value,
        result: &ResponsesResult,
        cancellation: CancellationToken,
    ) -> Result<Option<Value>, CoreError> {
        self.transport.check_current()?;
        let has_reasoning = result.output.iter().any(|item| item["type"] == "reasoning");
        let continuation = make_continuation(
            input,
            &self.identity,
            body,
            &result.output,
            &result.content,
            &result.calls,
        )?;
        if let Some(continuation) = &continuation {
            if has_reasoning && input.continuation_store.is_none() {
                return Err(CoreError::responses("continuation_callback_required"));
            }
            if let Some(store) = &input.continuation_store {
                tokio::select! {biased;
                    _=cancellation.cancelled()=>return Err(CoreError::new("cancelled","aborted")),
                    result=store.save(continuation.clone())=>result?,
                }
            }
        }
        if cancellation.is_cancelled() {
            return Err(CoreError::new("cancelled", "aborted"));
        }
        self.transport.check_current()?;
        Ok(continuation)
    }
}
#[async_trait]
impl ChatProvider for ResponsesProvider {
    fn supports_usage_cleanup(&self) -> bool {
        true
    }
    fn connection_identity(&self) -> Option<ConnectionIdentity> {
        Some(self.identity.clone())
    }
    fn id(&self) -> &str {
        &self.identity.provider_id
    }
    fn tracks_transport_requests(&self) -> bool {
        true
    }
    fn stream_chat(&self, input: ChatRequest, cancellation: CancellationToken) -> ProviderStream {
        self.events(input, cancellation)
    }
    async fn list_models(&self, cancellation: CancellationToken) -> Result<Vec<Value>, CoreError> {
        let response = dispatch(
            self.transport.as_ref(),
            &self.identity,
            "models",
            None,
            cancellation.clone(),
        )
        .await?;
        let payload = read_json(response, cancellation).await?;
        self.transport.check_current()?;
        let account = self.transport.auth_kind() == AuthKind::ChatGptAccount;
        let models = payload[if account { "models" } else { "data" }]
            .as_array()
            .ok_or_else(|| CoreError::responses("malformed_model_catalog"))?;
        let mut output = vec![];
        let fetched = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        for model in models {
            if account && model["visibility"] != "list" {
                continue;
            }
            let id = model[if account { "slug" } else { "id" }]
                .as_str()
                .filter(|value| !value.is_empty())
                .ok_or_else(|| CoreError::responses("malformed_model_catalog"))?;
            let label = model.get(if account { "display_name" } else { "name" });
            if label.is_some_and(|value| !value.is_string()) {
                return Err(CoreError::responses("malformed_model_catalog"));
            }
            output.push(json!({"providerId":self.id(),"modelId":id,"displayName":label.cloned().unwrap_or(Value::Null),"fetchedAt":fetched}));
        }
        Ok(output)
    }
}
fn response_usage(
    usage: &Value,
    model: &str,
    call_id: &str,
    final_for_call: bool,
) -> Result<Value, CoreError> {
    let mut mapped = json!({});
    for (source, target) in [
        ("input_tokens", "prompt_tokens"),
        ("output_tokens", "completion_tokens"),
        ("total_tokens", "total_tokens"),
    ] {
        if let Some(value) = usage.get(source) {
            mapped[target] = value.clone();
        }
    }
    usage_data(&mapped, model, call_id, final_for_call)
}
