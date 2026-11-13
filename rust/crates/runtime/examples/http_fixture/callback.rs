//! Fixture-only bridge to the unchanged TypeScript provider doubles.
use agentkit_core::{
    ChatProvider, ChatRequest, CoreError, ProviderEvent, ProviderStream, ToolCall,
};
use futures::{Stream, StreamExt};
use serde_json::{Value, json};
use std::pin::Pin;
use tokio_util::sync::CancellationToken;

pub struct Callback {
    pub url: String,
    pub token: String,
}
type Chunks = Pin<Box<dyn Stream<Item = Result<Vec<u8>, reqwest::Error>> + Send>>;
struct Bridge {
    request: Option<reqwest::RequestBuilder>,
    chunks: Option<Chunks>,
    bytes: Vec<u8>,
    cancel: CancellationToken,
    done: bool,
}
impl ChatProvider for Callback {
    fn id(&self) -> &str {
        "p1"
    }
    fn stream_chat(&self, input: ChatRequest, cancel: CancellationToken) -> ProviderStream {
        let request = reqwest::Client::new().post(&self.url).bearer_auth(&self.token).json(&json!({
            "runId":input.run_id,"model":input.model,"messages":input.messages,"tools":input.tools,
            "temperature":input.temperature,"maxOutputTokens":input.max_output_tokens
        }));
        let bridge = Bridge {
            request: Some(request),
            chunks: None,
            bytes: vec![],
            cancel,
            done: false,
        };
        Box::pin(futures::stream::unfold(bridge, |mut state| async move {
            if state.done {
                return None;
            }
            let result = state.next().await;
            match result {
                Ok(Some(event)) => Some((Ok(event), state)),
                Ok(None) => None,
                Err(error) => {
                    state.done = true;
                    Some((Err(error), state))
                }
            }
        }))
    }
}
impl Bridge {
    async fn next(&mut self) -> Result<Option<ProviderEvent>, CoreError> {
        if let Some(request) = self.request.take() {
            let response = tokio::select! {
                _ = self.cancel.cancelled() => return Ok(None),
                response = request.send() => response.map_err(bridge_error)?.error_for_status().map_err(bridge_error)?,
            };
            self.chunks = Some(Box::pin(
                response
                    .bytes_stream()
                    .map(|chunk| chunk.map(|bytes| bytes.to_vec())),
            ));
        }
        loop {
            if let Some(end) = self.bytes.iter().position(|b| *b == b'\n') {
                let line: Vec<_> = self.bytes.drain(..=end).collect();
                let text = std::str::from_utf8(&line).map_err(bridge_error)?;
                let value =
                    agentkit_contracts::parse_javascript_json(text).map_err(bridge_error)?;
                return decode(value).map(Some);
            }
            let chunk = tokio::select! {
                _ = self.cancel.cancelled() => return Ok(None),
                chunk = self.chunks.as_mut().unwrap().next() => chunk,
            };
            let Some(chunk) = chunk else {
                return if self.bytes.is_empty() {
                    Ok(None)
                } else {
                    Err(bridge_error("Unterminated provider bridge frame"))
                };
            };
            self.bytes.extend(chunk.map_err(bridge_error)?);
            if self.bytes.len() > 1_048_576 {
                return Err(bridge_error("Provider bridge frame exceeds limit"));
            }
        }
    }
}
fn bridge_error(error: impl std::fmt::Display) -> CoreError {
    CoreError::new("test_provider_bridge", &error.to_string())
}
fn decode(value: Value) -> Result<ProviderEvent, CoreError> {
    if let Some(error) = value.get("bridgeError") {
        return Err(CoreError::new(
            error["code"].as_str().unwrap_or("provider_error"),
            error["message"]
                .as_str()
                .unwrap_or("Provider callback failed"),
        ));
    }
    let kind = value["type"]
        .as_str()
        .ok_or_else(|| bridge_error("Missing provider event type"))?;
    let data = value["data"].clone();
    Ok(match kind {
        "run.started" => ProviderEvent::Started,
        "run.message.delta" => ProviderEvent::Delta(
            data["delta"]
                .as_str()
                .ok_or_else(|| bridge_error("Invalid delta"))?
                .into(),
        ),
        "run.message.completed" => ProviderEvent::Event {
            kind: kind.into(),
            data,
        },
        "run.tool.requested" => ProviderEvent::ToolRequested(ToolCall {
            id: data["toolCallId"]
                .as_str()
                .ok_or_else(|| bridge_error("Missing tool call id"))?
                .into(),
            name: data["toolName"]
                .as_str()
                .ok_or_else(|| bridge_error("Missing tool name"))?
                .into(),
            arguments_json: data["argumentsJson"]
                .as_str()
                .ok_or_else(|| bridge_error("Missing tool arguments"))?
                .into(),
        }),
        "run.failed" => {
            return Err(CoreError::new(
                data["errorCode"].as_str().unwrap_or("provider_error"),
                data["errorMessage"].as_str().unwrap_or("Provider failed"),
            ));
        }
        "run.completed" | "run.cancelled" => ProviderEvent::Activity,
        "run.usage" => ProviderEvent::Usage(data),
        _ => ProviderEvent::Event {
            kind: kind.into(),
            data,
        },
    })
}
