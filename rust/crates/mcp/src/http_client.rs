use crate::json_boundary;
use futures::{StreamExt, stream::BoxStream};
use http::{HeaderName, HeaderValue};
use rmcp::{model::*, transport::streamable_http_client::*};
use sse_stream::{Sse, SseStream};
use std::{collections::HashMap, sync::Arc};

#[derive(Clone)]
pub(crate) struct JsonHttpClient(
    pub reqwest::Client,
    pub Option<Arc<crate::ManagedOAuthProvider>>,
    pub Option<Arc<dyn McpHttpExecutor>>,
);

/// Trusted host/test HTTP port. Destination checks and dispatch fencing remain
/// in the adapter; implementations must release work when their future drops.
#[async_trait::async_trait]
pub trait McpHttpExecutor: Send + Sync {
    async fn execute(
        &self,
        request: reqwest::Request,
    ) -> Result<reqwest::Response, crate::McpClientError>;
}
type HttpError = StreamableHttpError<reqwest::Error>;
fn failure() -> HttpError {
    StreamableHttpError::UnexpectedServerResponse("Invalid bounded MCP response".into())
}

fn protect_stream(
    stream: BoxStream<'static, Result<Sse, SseError>>,
    oauth: Option<Arc<crate::ManagedOAuthProvider>>,
) -> BoxStream<'static, Result<Sse, SseError>> {
    stream
        .map(move |event| {
            event.and_then(|mut event| {
                if let Some(data) = &event.data
                    && !data.trim().is_empty()
                {
                    let raw = match &oauth {
                        Some(oauth) => oauth
                            .scrub(data)
                            .map_err(|error| SseError::Body(Box::new(error)))?,
                        None => data.clone(),
                    };
                    event.data = Some(
                        json_boundary::sdk_text(&raw)
                            .map_err(|error| SseError::Body(Box::new(error)))?,
                    );
                }
                Ok(event)
            })
        })
        .boxed()
}

fn sse(
    response: reqwest::Response,
    max: usize,
    oauth: Option<Arc<crate::ManagedOAuthProvider>>,
) -> BoxStream<'static, Result<Sse, SseError>> {
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
    protect_stream(SseStream::from_bytes_stream(bytes).boxed(), oauth)
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
        let auth = self.authorization(&uri, auth).await?;
        Ok(protect_stream(
            self.0
                .get_stream_with_max_sse_event_size(uri, session, last, auth, headers, max)
                .await?,
            self.1.clone(),
        ))
    }
    async fn delete_session(
        &self,
        uri: Arc<str>,
        session: Arc<str>,
        auth: Option<String>,
        headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), HttpError> {
        let auth = self.authorization(&uri, auth).await?;
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
        let auth = self.authorization(&uri, auth).await?;
        let mut request = self
            .0
            .post(uri.as_ref())
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json");
        if let Some(auth) = auth.as_deref() {
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
        let request = request
            .body(raw)
            .build()
            .map_err(StreamableHttpError::Client)?;
        if let ClientJsonRpcMessage::Request(envelope) = &message
            && let ClientRequest::CallToolRequest(call) = &envelope.request
            && let Some(guard) = call.extensions.get::<crate::client_request::WireDispatch>()
        {
            guard
                .check()
                .await
                .map_err(|error| StreamableHttpError::Io(std::io::Error::other(error)))?;
        }
        let response = match &self.2 {
            Some(executor) => executor
                .execute(request)
                .await
                .map_err(|error| StreamableHttpError::Io(std::io::Error::other(error)))?,
            None => self
                .0
                .execute(request)
                .await
                .map_err(StreamableHttpError::Client)?,
        };
        self.post_response(response, &message, session.is_some(), max, auth.as_deref())
            .await
    }
}
impl JsonHttpClient {
    async fn authorization(
        &self,
        uri: &str,
        existing: Option<String>,
    ) -> Result<Option<String>, HttpError> {
        let Some(oauth) = &self.1 else {
            return Ok(existing);
        };
        if uri != oauth.resource_url() || existing.is_some() {
            return Err(failure());
        }
        oauth
            .access_token()
            .await
            .map(Some)
            .map_err(|_| StreamableHttpError::AuthRequired(AuthRequiredError::new(String::new())))
    }
    async fn post_response(
        &self,
        mut response: reqwest::Response,
        message: &ClientJsonRpcMessage,
        attached: bool,
        max: usize,
        used_token: Option<&str>,
    ) -> Result<StreamableHttpPostResponse, HttpError> {
        let status = response.status();
        if let Some(oauth) = &self.1 {
            if status == reqwest::StatusCode::UNAUTHORIZED {
                let _ = oauth.unauthorized(used_token).await;
                return Err(StreamableHttpError::AuthRequired(AuthRequiredError::new(
                    String::new(),
                )));
            }
            if status == reqwest::StatusCode::FORBIDDEN {
                return Err(StreamableHttpError::InsufficientScope(
                    InsufficientScopeError::new(String::new(), None),
                ));
            }
        }
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
            return Ok(StreamableHttpPostResponse::Sse(
                sse(response, max, self.1.clone()),
                session,
            ));
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
        let raw = match &self.1 {
            Some(oauth) => oauth.scrub(raw).map_err(|_| failure())?,
            None => raw.to_owned(),
        };
        match json_boundary::sdk_decode::<ServerJsonRpcMessage>(&raw) {
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
