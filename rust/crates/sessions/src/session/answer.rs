//! Human requests of [`AgentSessionService`]: answers, plan approvals, denials
//! and standing approval grants.

use crate::{
    AgentInputKind, AgentInputRecord, AgentInputStatus, AnswerHumanRequest, AnswerReceipt,
    SessionBackend, SessionFault, SessionsError, SessionsResult, approval_answer_refusal,
    is_approval_request, parse_answer_expiry, plan_request_of, read_standing_key, standing_grants,
};
use agentkit_engine::TaskStatus;
use agentkit_kernel::sha256_hex;
use serde_json::{Map, Value};

use super::{
    AgentSessionService, AnswerInput, PlanAuthorization, PlanGrantIntent, answer_stale,
    answer_store_error,
};

impl<R: SessionBackend> AgentSessionService<R> {
    // -- human requests ---------------------------------------------------------

    /// Record the answer and re-admit the parked turn, then poke the queue.
    pub async fn answer(
        &self,
        input: &AnswerInput,
    ) -> SessionsResult<AnswerReceipt, SessionFault<R>> {
        let request = self
            .store()
            .get_input(input.request_id.clone())
            .await?
            .filter(|record| record.kind == AgentInputKind::HumanRequest)
            .ok_or_else(|| {
                SessionsError::InputNotFound(format!(
                    "Human request not found: {}",
                    input.request_id
                ))
            })?;
        self.require_session(&request.session_id).await?;
        let response_hash = sha256_hex(
            &serde_json::to_string(&input.response)
                .map_err(|error| SessionsError::invalid_input(error.to_string()))?,
        );
        // The TypeScript authority's transaction reads the answer's SHAPE
        // before any stale gate speaks (its `readDecision` runs first), so a
        // malformed approval answer to a cancelled request is
        // `invalid_input`, never `answer_stale`.
        if let Some(message) = approval_answer_refusal(&request.content, &input.response) {
            return Err(SessionsError::invalid_input(message));
        }
        // `assertRememberEligible` is the other pre-transaction refusal: a
        // PENDING approval that promises to remember an unofferable call is
        // refused up front, before the stale gates classify it.
        if request.status == AgentInputStatus::Pending
            && is_approval_request(&request.content)
            && input.response.get("remember") == Some(&Value::Bool(true))
            && read_standing_key(&request.content).is_none()
        {
            return Err(SessionsError::invalid_input(
                "This call is not eligible for a standing approval.",
            ));
        }
        // TS: `if (taskId === undefined) throw stale("it names no turn")` —
        // BEFORE the answered-replay branches, so an answered record that
        // names no turn is stale, never a replay.
        let task_id = request
            .target_task_id
            .clone()
            .ok_or_else(|| answer_stale(&input.request_id, "it names no turn"))?;
        // An already-answered request is an observation replay (or a
        // conflict the store refuses). It must not consult the plan-approval
        // policy: a one-shot decision would be spent twice, and a replay
        // must not depend on the grant provider still being reachable.
        if request.status == AgentInputStatus::Answered {
            let outcome = self
                .store()
                .answer_human_request(AnswerHumanRequest {
                    request_id: input.request_id.clone(),
                    answer_id: input.answer_id.clone(),
                    response: input.response.clone(),
                    response_hash,
                    now: self.now_ms(),
                    message_id: self.mint("msg_"),
                    plan_tool_names: vec![],
                    policy_denial: None,
                    granted_mode: None,
                })
                .await
                .map_err(answer_store_error)?;
            if outcome.enqueue.is_some() {
                self.wake();
            }
            return Ok(outcome.receipt);
        }
        let mut plan_tool_names = vec![];
        let mut policy_denial = None;
        let mut granted_mode = None;
        if let Some(policy) = &self.deps.plan_approval {
            plan_tool_names = policy.tool_names();
            // The human's decision is read BEFORE the grant policy: only an
            // approval escalates, so a denial never touches `granted_mode`
            // or `authorize`. A denial is the human's final word — the
            // request must persist (note included) even when the grant
            // service is unreachable, or the answer would fail, the request
            // stay pending and the parked turn wait forever. A response the
            // store will reject (no/other `decision`) also lands here: the
            // store validates the shape as its first write-free act.
            let approves =
                input.response.get("decision").and_then(Value::as_str) == Some("approve");
            if approves && let Some(plan) = plan_request_of(&request.content, &plan_tool_names) {
                let session_id = request.session_id.clone();
                let turn_id = request.target_task_id.clone().unwrap_or_default();
                let mode = policy
                    .granted_mode(&session_id)
                    .await
                    .map_err(|error| SessionsError::InvalidInputContext(Box::new(error)))?;
                if mode == "plan" || mode.is_empty() {
                    // The escalation is OUT of plan mode, never a destination.
                    return Err(SessionsError::invalid_input(
                        "a plan approval may not grant plan mode: it is the escalation out of it, not a destination.",
                    ));
                }
                match policy
                    .authorize(&PlanGrantIntent {
                        session_id: &session_id,
                        turn_id: &turn_id,
                        request_id: &request.input_id,
                        plan_digest: plan["planDigest"].as_str().unwrap_or_default(),
                        requested_mode: plan["requestedMode"].as_str(),
                    })
                    .await
                    .map_err(|error| SessionsError::InvalidInputContext(Box::new(error)))?
                {
                    PlanAuthorization::Allowed => granted_mode = Some(mode),
                    PlanAuthorization::Denied(reason) => {
                        policy_denial = Some(reason.unwrap_or_else(|| "denied".to_owned()));
                    }
                }
            }
        }
        // Session-layer stale gates, in the TypeScript transaction's order:
        // cancelled, then the turn's parkable state, then expiry. The store
        // re-checks all three inside its transaction (it stays authoritative
        // for persistence); these refuse before settlement so the code and
        // message match the TypeScript authority, and the races that only
        // the store can observe cross as `answer_stale` via
        // `answer_store_error`.
        if request.status == AgentInputStatus::Cancelled {
            return Err(answer_stale(&input.request_id, "it was cancelled"));
        }
        let task = self
            .store()
            .get_task(&task_id)
            .await
            .map_err(SessionsError::from)?;
        if !matches!(&task, Some(task) if task.status == TaskStatus::WaitingApproval) {
            return Err(answer_stale(
                &input.request_id,
                "its turn is no longer waiting",
            ));
        }
        if request
            .expires_at
            .as_deref()
            .and_then(parse_answer_expiry)
            .is_some_and(|expires| expires <= self.now_ms())
        {
            return Err(answer_stale(&input.request_id, "it expired"));
        }
        let outcome = self
            .store()
            .answer_human_request(AnswerHumanRequest {
                request_id: input.request_id.clone(),
                answer_id: input.answer_id.clone(),
                response: input.response.clone(),
                response_hash,
                now: self.now_ms(),
                message_id: self.mint("msg_"),
                plan_tool_names,
                policy_denial,
                granted_mode,
            })
            .await
            .map_err(answer_store_error)?;
        if outcome.enqueue.is_some() {
            self.wake();
        }
        Ok(outcome.receipt)
    }

    /// Approve exactly the call the request names (ADR 0019). `remember:
    /// true` also stores a standing approval for the same exact call, when
    /// the request offers one (`invalid_input` otherwise).
    pub async fn approve(
        &self,
        request_id: &str,
        answer_id: &str,
        note: Option<&str>,
        remember: Option<bool>,
    ) -> SessionsResult<AnswerReceipt, SessionFault<R>> {
        let mut response = Map::new();
        response.insert("decision".into(), "approve".into());
        if let Some(note) = note {
            response.insert("note".into(), note.into());
        }
        if let Some(remember) = remember {
            response.insert("remember".into(), remember.into());
        }
        self.answer(&AnswerInput {
            request_id: request_id.to_owned(),
            answer_id: answer_id.to_owned(),
            response: Value::Object(response),
        })
        .await
    }

    /// Answer the call with a denial result; it never runs.
    pub async fn deny(
        &self,
        request_id: &str,
        answer_id: &str,
        note: Option<&str>,
    ) -> SessionsResult<AnswerReceipt, SessionFault<R>> {
        let mut response = Map::new();
        response.insert("decision".into(), "deny".into());
        if let Some(note) = note {
            response.insert("note".into(), note.into());
        }
        self.answer(&AnswerInput {
            request_id: request_id.to_owned(),
            answer_id: answer_id.to_owned(),
            response: Value::Object(response),
        })
        .await
    }

    /// Every standing approval of the session, revoked ones included, newest
    /// first.
    pub async fn list_approval_grants(
        &self,
        session_id: &str,
    ) -> SessionsResult<Vec<Value>, SessionFault<R>> {
        let (chat, _) = self.require_session(session_id).await?;
        let mut grants = standing_grants(&chat.metadata);
        grants.reverse();
        Ok(grants)
    }

    /// Stop a standing approval from matching. Idempotent. Calls already
    /// running are not interrupted; the next matching call asks again.
    pub async fn revoke_approval_grant(
        &self,
        session_id: &str,
        grant_id: &str,
    ) -> SessionsResult<Value, SessionFault<R>> {
        self.require_session(session_id).await?;
        Ok(self
            .store()
            .revoke_standing_approval(session_id.to_owned(), grant_id.to_owned(), self.now_ms())
            .await?)
    }

    pub(super) fn plan_request_of(&self, record: &AgentInputRecord) -> Option<Value> {
        let names = self.deps.plan_approval.as_ref()?.tool_names();
        plan_request_of(&record.content, &names)
    }
}
