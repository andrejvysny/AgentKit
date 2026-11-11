//! Durable pending-interaction contracts for live human decisions on
//! running native turns (AGENTKIT-115 vocabulary; storage and CAS live in
//! the host/store).
//!
//! A live native turn keeps its task lease and heartbeat while a foreground
//! governed MCP call waits on a human; the parked `WaitingApproval` path is
//! never used for a running child. These records bind the exact request so
//! answers are compare-and-set and cannot be replayed across attempts.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// What the human is being asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum InteractionKind {
    /// Allow/deny a governed tool operation.
    Approval,
    /// A tool-defined question with a bounded response.
    Question,
}

/// Human decision for an interaction.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "camelCase", deny_unknown_fields)]
pub enum Decision {
    Allow,
    Deny,
    /// Response payload for a `Question` interaction.
    Respond {
        value: Value,
    },
}

/// A durable pending interaction for a RUNNING native attempt. Every field
/// participates in answer validation; the host rechecks authority again
/// immediately before dispatch (AGENTKIT-107 §6).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PendingInteraction {
    /// Host-owned interaction identity.
    pub interaction_id: String,
    pub task_id: String,
    pub attempt_id: String,
    /// Process generation that issued the request; answers from replaced
    /// processes are refused.
    pub process_generation: u64,
    /// Host invocation identity bound to the foreground MCP call.
    pub invocation_id: String,
    pub kind: InteractionKind,
    /// Exact request content (operation, arguments summary, question).
    pub content: Value,
    /// Canonical digest of `content`; answers must match it exactly.
    pub content_hash: String,
    /// Actor the request targets; cross-actor answers are refused.
    pub actor_id: String,
    pub issued_at_ms: i64,
    /// Bounded expiry (ms epoch); expiry resolves UI and MCP call alike.
    pub expires_at_ms: i64,
}

/// A submitted answer bound to every identity component (AGENTKIT-344 UX
/// consumes this shape through host APIs).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AnswerSubmission {
    pub interaction_id: String,
    /// Client-chosen idempotency identity for compare-and-set answers.
    pub answer_id: String,
    pub decision: Decision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Why an answer was refused. All rejections are durable-visible.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InteractionRejection {
    NotFound,
    CrossTask,
    StaleAttempt,
    StaleGeneration,
    Expired,
    /// A different answer already settled this interaction; the first
    /// settlement stands (idempotent duplicate acknowledgment is separate).
    AlreadyAnswered,
    /// The submission did not bind the exact request content hash.
    InputChanged,
    /// Cancel/quit/process replacement invalidated the interaction.
    Invalidated,
    /// Authority no longer permits the operation at answer time.
    AuthorityRevoked,
}

/// Result of a compare-and-set answer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "disposition", rename_all = "camelCase", deny_unknown_fields)]
pub enum AnswerDisposition {
    /// First accepted answer; the host revalidates authority before
    /// executing the intended foreground call.
    Accepted { answered_at_ms: i64 },
    /// Same `answer_id` redelivered; acknowledged without a second effect.
    Duplicate,
    /// Refused; the interaction remains pending unless terminal.
    Rejected { reason: InteractionRejection },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn record() -> PendingInteraction {
        PendingInteraction {
            interaction_id: "int_1".into(),
            task_id: "task_1".into(),
            attempt_id: "att_1".into(),
            process_generation: 3,
            invocation_id: "inv_1".into(),
            kind: InteractionKind::Approval,
            content: json!({"tool": "artifact_propose", "argsHash": "abc"}),
            content_hash: "hash".into(),
            actor_id: "user_1".into(),
            issued_at_ms: 1,
            expires_at_ms: 2,
        }
    }

    #[test]
    fn interaction_record_round_trips() {
        let value = serde_json::to_value(record()).unwrap();
        let back: PendingInteraction = serde_json::from_value(value).unwrap();
        assert_eq!(back, record());
    }

    #[test]
    fn answers_are_compare_and_set_shapes() {
        let answer = AnswerSubmission {
            interaction_id: "int_1".into(),
            answer_id: "ans_1".into(),
            decision: Decision::Allow,
            note: None,
        };
        let value = serde_json::to_value(&answer).unwrap();
        assert_eq!(value["decision"], json!({"decision": "allow"}));
        let disposition = AnswerDisposition::Rejected {
            reason: InteractionRejection::StaleGeneration,
        };
        let text = serde_json::to_value(disposition).unwrap();
        assert_eq!(text["reason"], "stale_generation");
    }

    #[test]
    fn question_answer_carries_value() {
        let answer = AnswerSubmission {
            interaction_id: "int_1".into(),
            answer_id: "ans_1".into(),
            decision: Decision::Respond {
                value: json!("blue"),
            },
            note: None,
        };
        let back: AnswerSubmission =
            serde_json::from_value(serde_json::to_value(&answer).unwrap()).unwrap();
        assert_eq!(back, answer);
    }
}
