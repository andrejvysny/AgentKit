use crate::{Call, Client, ClientError, RunState, SseDecoder, error::checked, sse::protocol};
use agentkit_contracts::RestOperation;
use futures::{StreamExt, stream::BoxStream};
use serde_json::Value;
use std::{collections::BTreeMap, time::Duration};

#[derive(Debug, Clone)]
pub struct StreamOptions {
    pub last_event_id: Option<String>,
    pub max_retries: usize,
    pub max_total_reconnects: usize,
    pub retry_delay: Duration,
    pub max_frame_bytes: usize,
}
impl Default for StreamOptions {
    fn default() -> Self {
        Self {
            last_event_id: None,
            max_retries: 5,
            max_total_reconnects: 50,
            retry_delay: Duration::from_millis(500),
            max_frame_bytes: 1_048_576,
        }
    }
}

impl Client {
    /// Drop the returned stream to cancel its socket, polling, and reconnect timer.
    pub fn stream_run(
        &self,
        run_id: &str,
        options: StreamOptions,
    ) -> BoxStream<'static, Result<Value, ClientError>> {
        let client = self.clone();
        let run_id = run_id.to_owned();
        Box::pin(async_stream::try_stream! {
            let mut state = RunState::default();
            let mut last_id = options.last_event_id;
            let mut attempts = 0;
            let mut reconnects = 0;
            let mut delay = options.retry_delay;
            loop {
                let connection = client.connection(&run_id,last_id.as_deref(),options.max_frame_bytes);
                futures::pin_mut!(connection);
                let mut settled = false;
                let mut failure = None;
                while let Some(item) = connection.next().await {
                    let frame = match item { Ok(frame) => frame, Err(error) => { failure = Some(error); break; } };
                    if let Some(ms) = frame.retry { delay = Duration::from_millis(ms.clamp(250,30_000)); }
                    if frame.event.as_deref() == Some("agentkit.stream.settled") {
                        if settled { Err(protocol("Duplicate settlement marker"))?; }
                        if frame.data.as_deref() != Some("{}") { Err(protocol("Invalid settlement marker"))?; }
                        settled = true; continue;
                    }
                    let Some(data) = frame.data else { continue; };
                    if settled { Err(protocol("Event received after settlement marker"))?; }
                    let event: Value = agentkit_contracts::parse_javascript_json(&data).map_err(|_| protocol("Invalid event JSON"))?;
                    agentkit_contracts::validate_schema("AiRunEventSchema",&event)
                        .map_err(|e| protocol(&e.to_string()))?;
                    if event["runId"].as_str() != Some(run_id.as_str()) { Err(protocol("Event belongs to another run"))?; }
                    if let Some(id) = frame.id
                        && event["eventId"].as_str() != Some(id.as_str()) { Err(protocol("SSE id does not match eventId"))?; }
                    if state.observe(&event)? {
                        last_id = state.last_event_id.clone(); attempts = 0;
                        yield event;
                    }
                }
                if settled && failure.is_none() { break; }
                let error = failure.unwrap_or_else(|| protocol("Run event stream ended without a settlement marker."));
                if matches!(error,ClientError::Http { .. }|ClientError::InvalidRequest(_))
                    || attempts >= options.max_retries || reconnects >= options.max_total_reconnects {
                    Err(error)?;
                }
                attempts += 1; reconnects += 1;
                tokio::time::sleep(delay).await;
            }
        })
    }

    fn connection(
        &self,
        run_id: &str,
        last_id: Option<&str>,
        limit: usize,
    ) -> BoxStream<'static, Result<crate::SseFrame, ClientError>> {
        let client = self.clone();
        let run_id = run_id.to_owned();
        let last_id = last_id.map(str::to_owned);
        Box::pin(async_stream::try_stream! {
            let mut call = Call { path: BTreeMap::from([("runId".into(),run_id)]), ..Call::default() };
            call.headers.insert("accept","text/event-stream".parse().unwrap());
            if let Some(id) = last_id {
                call.headers.insert("last-event-id",id.parse().map_err(|_| protocol("Invalid Last-Event-ID"))?);
            }
            let url = client.url(&RestOperation::StreamRun,&call)?;
            let response = checked(client.send(&RestOperation::StreamRun,&call,url).await?).await?;
            if !response.headers().get("content-type").and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.split(';').next() == Some("text/event-stream")) {
                Err(protocol("Expected text/event-stream response"))?;
            }
            let mut decoder = SseDecoder::new(limit);
            let mut bytes = response.bytes_stream();
            while let Some(chunk) = bytes.next().await {
                for frame in decoder.feed(&chunk?)? { yield frame; }
            }
            if decoder.has_partial_frame() { Err(protocol("SSE stream ended mid-frame"))?; }
            decoder.finish();
        })
    }
}
