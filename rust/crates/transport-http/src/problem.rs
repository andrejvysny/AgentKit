use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Problem {
    #[serde(rename = "type")]
    pub type_uri: Box<str>,
    pub title: String,
    pub status: u16,
    pub code: String,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
}

impl Problem {
    pub fn new(status: u16, code: &str, detail: impl Into<String>) -> Self {
        let title = match status {
            413 => "Content Too Large",
            422 => "Unprocessable Content",
            _ => StatusCode::from_u16(status)
                .ok()
                .and_then(|s| s.canonical_reason())
                .unwrap_or("Error"),
        };
        Self {
            type_uri: format!("https://agentkit.dev/problems/{code}").into_boxed_str(),
            title: title.into(),
            status,
            code: code.into(),
            detail: detail.into(),
            instance: None,
        }
    }

    pub fn host(code: &str, detail: impl Into<String>) -> Self {
        let status = match code {
            "not_found" => 404,
            "invalid_fork_point" | "invalid_regenerate" | "invalid_import" | "invalid_decision" => {
                400
            }
            "chat_busy"
            | "invalid_task_transition"
            | "invalid_proposal_transition"
            | "duplicate_task"
            | "duplicate_action_id"
            | "revision_conflict"
            | "lease_lost"
            | "seq_conflict"
            | "unknown_dependency" => 409,
            "usage_denied" => 429,
            "resume_unsupported" => 501,
            _ => 500,
        };
        Self::new(status, code, detail)
    }

    pub fn at(mut self, path: &str) -> Self {
        self.instance = Some(path.to_owned());
        self
    }
}

impl IntoResponse for Problem {
    fn into_response(mut self) -> Response {
        if self.status >= 500 {
            self.detail = "The server failed to handle the request.".into();
        }
        let status = StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let mut response = (status, Json(self)).into_response();
        response
            .headers_mut()
            .insert("content-type", "application/problem+json".parse().unwrap());
        response
    }
}
