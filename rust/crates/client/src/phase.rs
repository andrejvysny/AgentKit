use crate::{ClientError, sse::protocol};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunPhase {
    Queued,
    Running,
    Streaming,
    Settling,
    Interrupted,
    Incomplete,
    WaitingApproval,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Default)]
pub struct RunState {
    pub text: String,
    pub last_seq: Option<f64>,
    pub last_event_id: Option<String>,
    pub status: Option<String>,
    pub started_new_pass: bool,
    streaming: bool,
    seen: bool,
    outcome: Option<RunPhase>,
}

impl RunState {
    /// Returns false for replayed numeric sequences without moving the cursor.
    pub fn observe(&mut self, event: &Value) -> Result<bool, ClientError> {
        let seq = agentkit_contracts::javascript_number(&event["seq"])
            .filter(|number| number.is_finite())
            .ok_or_else(|| protocol("Event seq must be a finite number"))?;
        let id = event["eventId"]
            .as_str()
            .ok_or_else(|| protocol("Event eventId is required"))?;
        let kind = event["type"]
            .as_str()
            .ok_or_else(|| protocol("Event type is required"))?;
        if let Some(previous) = self.last_seq
            && seq <= previous
        {
            return Ok(false);
        }
        self.started_new_pass = (kind == "run.warning" && event["data"]["code"] == "retry_pass")
            || (kind == "run.started" && (self.streaming || self.outcome.is_some()));
        if self.started_new_pass {
            self.text.clear();
            self.outcome = None;
        }
        self.seen = true;
        if matches!(kind, "run.started" | "run.message.delta") {
            self.streaming = true;
        }
        if kind == "run.message.delta"
            && let Some(delta) = event["data"]["delta"].as_str()
        {
            self.text.push_str(delta);
        }
        self.outcome = match kind {
            "run.completed" if event["data"]["finishReason"] == "incomplete" => {
                Some(RunPhase::Incomplete)
            }
            "run.completed" => Some(RunPhase::Completed),
            "run.failed" => Some(RunPhase::Failed),
            "run.cancelled" => Some(RunPhase::Cancelled),
            _ => self.outcome,
        };
        self.last_seq = Some(seq);
        self.last_event_id = Some(id.into());
        Ok(true)
    }

    pub fn phase(&self) -> RunPhase {
        match self.status.as_deref() {
            Some("interrupted") => return RunPhase::Interrupted,
            Some("waiting_approval") => return RunPhase::WaitingApproval,
            Some("failed") => return RunPhase::Failed,
            Some("cancelled") => return RunPhase::Cancelled,
            Some("completed") => {
                return if self.outcome == Some(RunPhase::Incomplete) {
                    RunPhase::Incomplete
                } else {
                    RunPhase::Completed
                };
            }
            _ => {}
        }
        if self.outcome.is_some() {
            return RunPhase::Settling;
        }
        if self.streaming {
            return RunPhase::Streaming;
        }
        if self.status.as_deref() == Some("running") || self.seen {
            RunPhase::Running
        } else {
            RunPhase::Queued
        }
    }

    pub fn outcome(&self) -> Option<RunPhase> {
        self.outcome
    }
}
