use crate::{Actor, Backend, Problem};
use axum::response::{
    IntoResponse, Response,
    sse::{Event, Sse},
};
use serde_json::Value;
use std::{io, sync::Arc, time::Duration};

#[derive(Debug, Clone)]
pub struct StreamOptions {
    pub read_batch_size: usize,
    pub poll_interval: Duration,
    pub heartbeat_interval: Duration,
    pub retry_hint: Duration,
    pub max_event_bytes: usize,
}
impl Default for StreamOptions {
    fn default() -> Self {
        Self {
            read_batch_size: 100,
            poll_interval: Duration::from_millis(100),
            heartbeat_interval: Duration::from_secs(15),
            retry_hint: Duration::from_millis(500),
            max_event_bytes: 1_048_576,
        }
    }
}

pub(crate) async fn response(
    backend: Arc<dyn Backend>,
    actor: Actor,
    run_id: &str,
    last_id: Option<&str>,
    options: StreamOptions,
) -> Result<Response, Problem> {
    if backend.run_status(&actor, run_id).await?.is_none() {
        return Err(Problem::new(
            404,
            "not_found",
            format!("Run not found: {run_id}"),
        ));
    }
    let batch_size = options.read_batch_size.max(1);
    let start = resolve_cursor(&*backend, &actor, run_id, last_id, batch_size).await?;
    let run_id = run_id.to_owned();
    let heartbeat = options.heartbeat_interval.max(Duration::from_millis(1));
    let stream = async_stream::try_stream! {
        yield Event::default().retry(options.retry_hint);
        let mut cursor = start;
        let mut settled = false;
        loop {
            let batch = backend.events(&actor,&run_id,cursor,batch_size).await.map_err(failure)?;
            if batch.len() > batch_size { Err(io::Error::other("Backend exceeded event page limit"))?; }
            let count = batch.len();
            for event in batch {
                let seq = sequence(&event).map_err(failure)?;
                if cursor.is_some_and(|previous| seq <= previous) { Err(io::Error::other("Durable event sequence is not strictly increasing"))?; }
                let data = agentkit_contracts::stringify_javascript_value(&event).map_err(io::Error::other)?;
                if data.len() > options.max_event_bytes { Err(io::Error::other("Event exceeds stream frame limit"))?; }
                let id = clean(event["eventId"].as_str().ok_or_else(|| io::Error::other("Missing eventId"))?);
                let kind = clean(event["type"].as_str().ok_or_else(|| io::Error::other("Missing event type"))?);
                yield Event::default().id(id).event(kind).data(data);
                cursor = Some(seq);
            }
            if count == batch_size { continue; }
            if settled { yield Event::default().event("agentkit.stream.settled").data("{}"); break; }
            let status = backend.run_status(&actor,&run_id).await.map_err(failure)?;
            settled = status.is_none_or(|s| matches!(s.as_str(), "completed"|"failed"|"cancelled"|"interrupted"));
            if !settled { tokio::time::sleep(options.poll_interval).await; }
        }
    };
    let stream: std::pin::Pin<Box<dyn futures::Stream<Item = Result<Event, io::Error>> + Send>> =
        Box::pin(stream);
    let mut response = Sse::new(stream)
        .keep_alive(
            axum::response::sse::KeepAlive::new()
                .interval(heartbeat)
                .text("hb"),
        )
        .into_response();
    response
        .headers_mut()
        .insert("cache-control", "no-cache, no-transform".parse().unwrap());
    Ok(response)
}

async fn resolve_cursor(
    backend: &dyn Backend,
    actor: &Actor,
    run_id: &str,
    last_id: Option<&str>,
    limit: usize,
) -> Result<Option<f64>, Problem> {
    let Some(id) = last_id.filter(|s| !s.trim().is_empty()) else {
        return Ok(None);
    };
    let mut cursor = None;
    loop {
        let events = backend.events(actor, run_id, cursor, limit).await?;
        if events.len() > limit {
            return Err(Problem::host(
                "internal_error",
                "Backend exceeded event page limit",
            ));
        }
        let count = events.len();
        for event in events {
            let seq = sequence(&event)?;
            if cursor.is_some_and(|previous| seq <= previous) {
                return Err(Problem::host(
                    "seq_conflict",
                    "Durable event sequence is not strictly increasing",
                ));
            }
            cursor = Some(seq);
            if event["eventId"].as_str() == Some(id) {
                return Ok(cursor);
            }
        }
        if count < limit {
            return Ok(None);
        }
    }
}

fn sequence(event: &Value) -> Result<f64, Problem> {
    agentkit_contracts::javascript_number(&event["seq"])
        .filter(|number| number.is_finite())
        .ok_or_else(|| Problem::host("seq_conflict", "Event sequence is not a finite number"))
}
fn clean(value: &str) -> String {
    value.replace(['\r', '\n', '\0'], "")
}
fn failure(problem: Problem) -> io::Error {
    io::Error::other(format!("{}: {}", problem.code, problem.detail))
}
