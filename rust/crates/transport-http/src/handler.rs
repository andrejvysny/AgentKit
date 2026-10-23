use crate::{Backend, Idempotency, OperationRequest, Problem, StreamOptions, derive_task_id};
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::any,
};
use serde_json::Value;
use std::{collections::BTreeMap, sync::Arc};

#[derive(Debug, Clone)]
pub struct Config {
    pub base_path: String,
    pub max_body_bytes: usize,
    pub max_query_bytes: usize,
    pub streaming: StreamOptions,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            base_path: String::new(),
            max_body_bytes: 1_048_576,
            max_query_bytes: 16_384,
            streaming: StreamOptions::default(),
        }
    }
}

#[derive(Clone)]
struct Service {
    backend: Arc<dyn Backend>,
    config: Config,
}

pub fn router(backend: Arc<dyn Backend>, mut config: Config) -> Router {
    config.base_path = config.base_path.trim().trim_end_matches('/').to_owned();
    if !config.base_path.is_empty() && !config.base_path.starts_with('/') {
        config.base_path.insert(0, '/');
    }
    Router::new()
        .fallback(any(handle))
        .with_state(Service { backend, config })
}

async fn handle(State(service): State<Service>, request: Request) -> Response {
    let path = request.uri().path().to_owned();
    match route_request(&service, request).await {
        Ok(response) => response,
        Err(problem) => problem.at(&path).into_response(),
    }
}

async fn route_request(service: &Service, request: Request) -> Result<Response, Problem> {
    let path = request.uri().path();
    let routed = if service.config.base_path.is_empty() {
        path
    } else {
        path.strip_prefix(&format!("{}/", service.config.base_path))
            .map(|s| &path[path.len() - s.len() - 1..])
            .ok_or_else(|| no_route(&request))?
    };
    let matches: Vec<_> = crate::routes()
        .iter()
        .filter_map(|route| crate::routes::match_path(route, routed).map(|params| (route, params)))
        .collect();
    let Some((route, params)) = matches
        .iter()
        .find(|(r, _)| r.method == request.method().as_str())
    else {
        if matches.is_empty() {
            return Err(no_route(&request));
        }
        let allow = matches
            .iter()
            .map(|(r, _)| r.method.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let mut response = Problem::new(
            405,
            "method_not_allowed",
            format!(
                "{} is not allowed on {path}; allowed: {allow}.",
                request.method()
            ),
        )
        .at(path)
        .into_response();
        response
            .headers_mut()
            .insert("allow", allow.parse().unwrap());
        return Ok(response);
    };
    let query = parse_query(request.uri().query(), service.config.max_query_bytes)?;
    let operation = route.operation.clone();
    let headers = request.headers().clone();
    if headers
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .is_some_and(|length| length > service.config.max_body_bytes as u64)
    {
        return Err(Problem::new(
            413,
            "body_too_large",
            format!(
                "Request body exceeds the configured limit of {} bytes.",
                service.config.max_body_bytes
            ),
        ));
    }
    let bytes = to_bytes(request.into_body(), service.config.max_body_bytes)
        .await
        .map_err(|_| {
            Problem::new(
                413,
                "body_too_large",
                format!(
                    "Request body exceeds the configured limit of {} bytes.",
                    service.config.max_body_bytes
                ),
            )
        })?;
    let actor = service.backend.authenticate(&headers).await?;
    let mut input = OperationRequest {
        actor,
        operation,
        params: params.clone(),
        query,
        body: Value::Null,
        idempotency: None,
    };
    if input.operation != "getVersion" {
        service.backend.authorize(&input).await?;
    }
    if matches!(
        input.operation.as_str(),
        "submitMessage" | "regenerateMessage"
    ) {
        idempotency(&input, &headers)?;
    }
    input.body = parse_body(&input.operation, &bytes)?;
    crate::validation::validate_query(&input.operation, &input.query)?;
    crate::validation::validate_body(&input.operation, &input.body)?;
    if input.operation == "updateProvider" {
        crate::validation::provider_id(&input.params["providerId"])?;
    }
    input.idempotency = idempotency(&input, &headers)?;
    if input.operation == "streamRun" {
        return crate::stream::response(
            service.backend.clone(),
            input.actor,
            &input.params["runId"],
            headers.get("last-event-id").and_then(|v| v.to_str().ok()),
            service.config.streaming.clone(),
        )
        .await;
    }
    let output = service.backend.dispatch(input).await?;
    let status = StatusCode::from_u16(output.status)
        .map_err(|_| Problem::host("internal_error", "Invalid backend status"))?;
    if status == StatusCode::NO_CONTENT {
        return Ok((status, Body::empty()).into_response());
    }
    Ok((
        status,
        axum::Json(agentkit_contracts::javascript_json_value(&output.body)),
    )
        .into_response())
}

fn parse_query(raw: Option<&str>, limit: usize) -> Result<BTreeMap<String, String>, Problem> {
    let raw = raw.unwrap_or("");
    if raw.len() > limit {
        return Err(Problem::new(
            400,
            "invalid_request",
            "Query string exceeds configured limit.",
        ));
    }
    let mut result = BTreeMap::new();
    for (key, value) in url::form_urlencoded::parse(raw.as_bytes()) {
        result
            .entry(key.into_owned())
            .or_insert_with(|| value.into_owned());
    }
    Ok(result)
}

fn parse_body(operation: &str, bytes: &[u8]) -> Result<Value, Problem> {
    if crate::validation::body_schema(operation).is_none() {
        return Ok(Value::Null);
    }
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(serde_json::json!({}));
    }
    let text = std::str::from_utf8(bytes).map_err(|error| {
        Problem::new(
            400,
            "invalid_body",
            format!("Request body is not valid JSON: {error}"),
        )
    })?;
    let value = agentkit_contracts::parse_javascript_json(text).map_err(|e| {
        Problem::new(
            400,
            "invalid_body",
            format!("Request body is not valid JSON: {e}"),
        )
    })?;
    if !value.is_object() {
        return Err(Problem::new(
            400,
            "invalid_body",
            "Request body must be a JSON object.",
        ));
    }
    let mut pending = vec![(&value, 1)];
    while let Some((value, depth)) = pending.pop() {
        let children: Vec<_> = match value {
            Value::Object(m) => m.values().collect(),
            Value::Array(a) => a.iter().collect(),
            _ => continue,
        };
        if depth > 64 {
            return Err(Problem::new(
                400,
                "invalid_body",
                "Request body is nested more than 64 levels deep.",
            ));
        }
        pending.extend(children.into_iter().map(|child| (child, depth + 1)));
    }
    Ok(value)
}

fn idempotency(
    input: &OperationRequest,
    headers: &HeaderMap,
) -> Result<Option<Idempotency>, Problem> {
    if !matches!(
        input.operation.as_str(),
        "submitMessage" | "regenerateMessage"
    ) {
        return Ok(None);
    }
    let key = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            Problem::new(
                400,
                "idempotency_key_required",
                "Submitting or regenerating a message requires an `Idempotency-Key` header.",
            )
        })?;
    let message = input.params.get("messageId").map(String::as_str);
    Ok(Some(Idempotency {
        key: key.into(),
        task_id: derive_task_id(&input.params["chatId"], message, key),
        fingerprint: crate::idempotency::fingerprint(&input.operation, &input.body),
    }))
}

fn no_route(request: &Request) -> Problem {
    Problem::new(
        404,
        "not_found",
        format!(
            "No route for {} {}.",
            request.method(),
            request.uri().path()
        ),
    )
}
