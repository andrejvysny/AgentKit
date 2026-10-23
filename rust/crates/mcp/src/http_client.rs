use crate::json_boundary;
use futures::{StreamExt, stream::BoxStream};
use http::{HeaderName, HeaderValue};
use rmcp::{model::*, transport::streamable_http_client::*};
use sse_stream::{Sse, SseStream};
use std::{collections::HashMap, sync::Arc};

#[derive(Clone)]
pub(crate) struct JsonHttpClient(pub reqwest::Client);
type HttpError = StreamableHttpError<reqwest::Error>;
fn failure() -> HttpError {
    StreamableHttpError::UnexpectedServerResponse("Invalid bounded MCP response".into())
}

fn protect_stream(
    stream: BoxStream<'static, Result<Sse, SseError>>,
) -> BoxStream<'static, Result<Sse, SseError>> {
    stream
        .map(|event| {
            event.and_then(|mut event| {
                if let Some(data) = &event.data
                    && !data.trim().is_empty()
                {
                    event.data = Some(
                        json_boundary::sdk_text(data)
                            .map_err(|error| SseError::Body(Box::new(error)))?,
                    );
                }
                Ok(event)
            })
        })
        .boxed()
}

fn sse(response: reqwest::Response, max: usize) -> BoxStream<'static, Result<Sse, SseError>> {
    let bytes = response
        .bytes_stream()
        .scan((0_usize, false, false, false), move |state, item| {
            if state.3 {
                return std::future::ready(None);
            }
            let result = item.map_err(std::io::Error::other).and_then(|bytes| {
                for byte in &bytes {
                    // CRLF is one line ending, including across response chunks.
                    if state.2 && *byte == b'\n' {
                        state.2 = false;
                        continue;
                    }
                    state.2 = *byte == b'\r';
                    state.0 = state.0.saturating_add(1);
                    if matches!(*byte, b'\r' | b'\n') {
                        if !state.1 {
                            state.0 = 0;
                        }
                        state.1 = false;
                    } else {
                        state.1 = true;
                    }
                    if state.0 > max {
                        state.3 = true;
                        return Err(std::io::Error::other("MCP SSE event exceeds bound"));
                    }
                }
                Ok(bytes)
            });
            std::future::ready(Some(result))
        });
    protect_stream(SseStream::from_bytes_stream(bytes).boxed())
}

impl StreamableHttpClient for JsonHttpClient {
    type Error = reqwest::Error;
    async fn get_stream(
        &self,
        uri: Arc<str>,
        session: Option<Arc<str>>,
        last: Option<String>,
        auth: Option<String>,
        headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<BoxStream<'static, Result<Sse, SseError>>, HttpError> {
        self.get_stream_with_max_sse_event_size(uri, session, last, auth, headers, 1_048_576)
            .await
    }
    async fn get_stream_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        session: Option<Arc<str>>,
        last: Option<String>,
        auth: Option<String>,
        headers: HashMap<HeaderName, HeaderValue>,
        max: usize,
    ) -> Result<BoxStream<'static, Result<Sse, SseError>>, HttpError> {
        Ok(protect_stream(
            self.0
                .get_stream_with_max_sse_event_size(uri, session, last, auth, headers, max)
                .await?,
        ))
    }
    async fn delete_session(
        &self,
        uri: Arc<str>,
        session: Arc<str>,
        auth: Option<String>,
        headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), HttpError> {
        self.0.delete_session(uri, session, auth, headers).await
    }
    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session: Option<Arc<str>>,
        auth: Option<String>,
        headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, HttpError> {
        self.post_message_with_max_sse_event_size(uri, message, session, auth, headers, 1_048_576)
            .await
    }
    async fn post_message_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session: Option<Arc<str>>,
        auth: Option<String>,
        headers: HashMap<HeaderName, HeaderValue>,
        max: usize,
    ) -> Result<StreamableHttpPostResponse, HttpError> {
        let mut request = self
            .0
            .post(uri.as_ref())
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json");
        if let Some(auth) = auth {
            request = request.bearer_auth(auth);
        }
        if let Some(session) = &session {
            request = request.header("mcp-session-id", session.as_ref());
        }
        for (name, value) in headers {
            // Match SDK header validation while retaining its injected version.
            if matches!(name.as_str(), "accept" | "mcp-session-id" | "last-event-id") {
                return Err(StreamableHttpError::ReservedHeaderConflict(
                    name.to_string(),
                ));
            }
            request = request.header(name, value);
        }
        let raw = json_boundary::wire_text(&message).map_err(StreamableHttpError::Deserialize)?;
        let response = request
            .body(raw)
            .send()
            .await
            .map_err(StreamableHttpError::Client)?;
        self.post_response(response, &message, session.is_some(), max)
            .await
    }
}
impl JsonHttpClient {
    async fn post_response(
        &self,
        mut response: reqwest::Response,
        message: &ClientJsonRpcMessage,
        attached: bool,
        max: usize,
    ) -> Result<StreamableHttpPostResponse, HttpError> {
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND && attached {
            return Err(StreamableHttpError::SessionExpired);
        }
        if matches!(status.as_u16(), 202 | 204) {
            return Ok(StreamableHttpPostResponse::Accepted);
        }
        let session = response
            .headers()
            .get("mcp-session-id")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let mime = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_owned();
        if status.is_success() && mime.starts_with("text/event-stream") {
            return Ok(StreamableHttpPostResponse::Sse(sse(response, max), session));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(StreamableHttpError::Client)?
        {
            if bytes.len().saturating_add(chunk.len()) > 4_194_304 {
                return Err(failure());
            }
            bytes.extend_from_slice(&chunk);
        }
        let notification = !matches!(message, ClientJsonRpcMessage::Request(_));
        if status.is_success() && notification && bytes.is_empty() {
            return Ok(StreamableHttpPostResponse::Accepted);
        }
        if !mime.starts_with("application/json") {
            return Err(failure());
        }
        let raw = std::str::from_utf8(&bytes).map_err(|_| failure())?;
        match json_boundary::sdk_decode::<ServerJsonRpcMessage>(raw) {
            Ok(message)
                if status.is_success() || matches!(message, ServerJsonRpcMessage::Error(_)) =>
            {
                Ok(StreamableHttpPostResponse::Json(message, session))
            }
            Err(_) if status.is_success() && notification => {
                Ok(StreamableHttpPostResponse::Accepted)
            }
            _ => Err(failure()),
        }
    }
}
