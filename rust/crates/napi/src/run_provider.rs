use crate::{errors, jsbridge::JsBridge};
use agentkit_core::{
    CancellationToken, ChatProvider, ChatRequest, CoreError, ExecutionBudget, ProviderEvent,
    ProviderStream, ToolCall,
};
use futures::stream;
use napi::bindgen_prelude::*;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, LazyLock, Mutex},
};
use tokio::sync::mpsc;

type StreamEvent = (
    std::result::Result<ProviderEvent, CoreError>,
    Arc<tokio::sync::Notify>,
);
type Sender = mpsc::Sender<StreamEvent>;
#[derive(Clone)]
struct StreamEntry {
    sender: Sender,
    budget: Option<Arc<ExecutionBudget>>,
    cancellation: CancellationToken,
}
static STREAMS: LazyLock<Mutex<HashMap<String, StreamEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[napi]
pub async fn push_run_provider_event(call_id: String, event_json: String) -> Result<bool> {
    let event: Value = serde_json::from_str(&event_json)
        .map_err(|e| errors::stable("invalid_input", e.to_string(), None))?;
    let entry = STREAMS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&call_id)
        .cloned();
    let Some(entry) = entry else { return Ok(false) };
    let ack = Arc::new(tokio::sync::Notify::new());
    if entry
        .sender
        .send((map_event(event), ack.clone()))
        .await
        .is_err()
    {
        return Ok(false);
    }
    tokio::select! {
        _ = ack.notified() => Ok(!entry.cancellation.is_cancelled()),
        _ = entry.cancellation.cancelled() => Ok(false),
    }
}
#[napi]
pub async fn reserve_run_provider_request(call_id: String) -> Result<()> {
    let entry = STREAMS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&call_id)
        .cloned()
        .ok_or_else(|| {
            errors::stable("invalid_input", "Provider call is no longer active", None)
        })?;
    if entry.cancellation.is_cancelled() {
        return Err(errors::stable(
            "invalid_input",
            "Provider call is no longer active",
            None,
        ));
    }
    if let Some(budget) = entry.budget
        && let Err(error) = budget.consume("providerRequests").await
    {
        budget.fail(error.clone());
        return Err(errors::stable(&error.code, error.message, None));
    }
    if entry.cancellation.is_cancelled() {
        return Err(errors::stable(
            "invalid_input",
            "Provider call is no longer active",
            None,
        ));
    }
    Ok(())
}
#[napi]
pub fn is_run_callback_active(call_id: String) -> bool {
    STREAMS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&call_id)
        .is_some_and(|entry| !entry.cancellation.is_cancelled())
        || crate::jsbridge::active(&call_id)
}
#[napi]
pub fn end_run_provider(call_id: String) -> bool {
    STREAMS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&call_id)
        .is_some()
}
fn map_event(event: Value) -> std::result::Result<ProviderEvent, CoreError> {
    let kind = event["type"].as_str().unwrap_or("");
    let data = event["data"].clone();
    match kind {
        "run.started" => Ok(ProviderEvent::Started),
        "run.failed" => Err(CoreError::new(
            data["errorCode"].as_str().unwrap_or(""),
            data["errorMessage"].as_str().unwrap_or("Provider failed"),
        )),
        "run.cancelled" => Err(CoreError::new("cancelled", "aborted")),
        "run.message.delta" => Ok(ProviderEvent::Delta(
            data["delta"].as_str().unwrap_or("").into(),
        )),
        "run.tool.requested" => Ok(ProviderEvent::ToolRequested(ToolCall {
            id: data["toolCallId"].as_str().unwrap_or("").into(),
            name: data["toolName"].as_str().unwrap_or("").into(),
            arguments_json: data["argumentsJson"].as_str().unwrap_or("{}").into(),
        })),
        "run.usage" => Ok(ProviderEvent::Usage(data)),
        "run.warning" => Ok(ProviderEvent::Warning {
            code: data["code"].as_str().unwrap_or("").into(),
            message: data["message"].as_str().unwrap_or("").into(),
        }),
        "run.message.completed" | "run.message.reasoning-delta" | "run.context.admitted" => {
            Ok(ProviderEvent::Event {
                kind: kind.into(),
                data,
            })
        }
        "transport.activity" => Ok(ProviderEvent::Activity),
        "provider.continuation" => Ok(ProviderEvent::Continuation(data)),
        _ => Err(CoreError::new(
            "invalid_input",
            "Unsupported provider event",
        )),
    }
}
struct StreamState {
    id: String,
    receiver: mpsc::Receiver<StreamEvent>,
    pending_ack: Option<Arc<tokio::sync::Notify>>,
    bridge: Arc<JsBridge>,
}
impl Drop for StreamState {
    fn drop(&mut self) {
        if let Some(ack) = self.pending_ack.take() {
            ack.notify_one();
        }
        end_run_provider(self.id.clone());
        self.bridge
            .deliver_message(json!({"kind":"cancel", "callId":self.id}).to_string());
    }
}

pub struct Provider {
    pub bridge: Arc<JsBridge>,
    pub tracks_transport_requests: bool,
}
impl ChatProvider for Provider {
    fn id(&self) -> &str {
        "js"
    }
    fn tracks_transport_requests(&self) -> bool {
        self.tracks_transport_requests
    }
    fn stream_chat(&self, request: ChatRequest, cancellation: CancellationToken) -> ProviderStream {
        let (sender, receiver) = mpsc::channel(16);
        let id = uuid::Uuid::new_v4().to_string();
        STREAMS.lock().unwrap_or_else(|e| e.into_inner()).insert(
            id.clone(),
            StreamEntry {
                sender,
                budget: request.budget.clone(),
                cancellation,
            },
        );
        let payload = json!({"kind":"provider","callId":id,"request":{"runId":request.run_id,"model":request.model,"messages":request.messages,"tools":request.tools,"temperature":request.temperature,"maxOutputTokens":request.max_output_tokens,"continuation":request.continuation,"continuationScope":request.continuation_scope,"continuationRequired":request.continuation_required}}).to_string();
        let status = self.bridge.deliver_message(payload);
        if status != napi::Status::Ok {
            end_run_provider(id.clone());
        }
        let state = StreamState {
            id,
            receiver,
            pending_ack: None,
            bridge: self.bridge.clone(),
        };
        Box::pin(stream::unfold(state, |mut state| async move {
            if let Some(ack) = state.pending_ack.take() {
                ack.notify_one();
            }
            state.receiver.recv().await.map(|(event, ack)| {
                state.pending_ack = Some(ack);
                (event, state)
            })
        }))
    }
}
