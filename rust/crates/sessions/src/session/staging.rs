//! The staging contract of [`AgentSessionService`]: how a queued input becomes
//! a turn, built identically on every promotion path.

use crate::{
    AgentInputKind, MessageInput, SessionBackend, SessionFault, SessionsError, SessionsResult,
    StagedCandidate, TurnStaging,
};
use agentkit_engine::SubmitTask;
use serde_json::{Map, Value, json};

use super::{
    AgentSessionService, read_input_content, session_content_selection, session_turn_kind,
    turn_config, turn_id_for,
};

impl<R: SessionBackend> AgentSessionService<R> {
    // -- internals -----------------------------------------------------------

    /// THE staging contract for a queued input: the turn it becomes, built
    /// exactly as a submission made right now would build it — the CURRENT
    /// session descriptor's config (`turn_payload_base`), this session's turn
    /// kind, and ids derived from the input itself (`turn:<inputId>`,
    /// `steer:<inputId>` for a steer, `msg_q:`/`msg_a:` for a follow-up) so
    /// every path that promotes one input writes one identical receipt.
    ///
    /// Submission, an explicit queue flush, settlement promotion and the
    /// recovery of a half-admitted input all stage through here; the store's
    /// [`crate::TurnStaging`] carries the same config into the one
    /// promotion the host's snapshot cannot pre-stage. `None` is the staging
    /// filter: unusable content stays Pending and visible.
    pub(super) async fn stage_candidate(
        &self,
        session_id: &str,
        descriptor: &Value,
        input_id: &str,
        kind: AgentInputKind,
        content: &Value,
    ) -> SessionsResult<Option<StagedCandidate>, SessionFault<R>> {
        // Capture the frozen stanza from the durable envelope BEFORE the
        // content shadow below (read_input_content unwraps the envelope).
        let frozen = content
            .get("frozen")
            .and_then(Value::as_object)
            .cloned()
            .filter(|frozen| !frozen.is_empty());
        let Some(content) = read_input_content(content) else {
            return Ok(None);
        };
        let user_message_id = if kind == AgentInputKind::Steer {
            format!("steer:{input_id}")
        } else {
            format!("msg_q:{input_id}")
        };
        let assistant_message_id = format!("msg_a:{input_id}");
        let task_id = turn_id_for(input_id);
        let mut payload = self.turn_payload_base(session_id, descriptor).await?;
        // A queued input's frozen native stanza (from the durable envelope)
        // lands verbatim — the flushed turn carries exactly the bytes the
        // sender submitted, validated against the same admission contract.
        if let Some(frozen) = &frozen {
            self.check_frozen(Some(frozen), descriptor)?;
            for (key, value) in frozen {
                payload.insert(key.clone(), value.clone());
            }
        }
        payload.insert(
            "assistantMessageId".into(),
            assistant_message_id.clone().into(),
        );
        payload.insert("userMessageId".into(), user_message_id.clone().into());
        let task = SubmitTask {
            task_id: task_id.clone(),
            kind: session_turn_kind(descriptor),
            scope_id: session_id.to_owned(),
            payload: Value::Object(payload),
            priority: 0,
            now: self.now_ms(),
            available_at: None,
            depends_on: vec![],
            parent_task_id: None,
            exclusive_scope: true,
        };
        Ok(Some(StagedCandidate {
            input_id: input_id.to_owned(),
            task,
            messages: self.stage_messages(
                session_id,
                &task_id,
                &content,
                &user_message_id,
                &assistant_message_id,
                None,
            ),
        }))
    }

    /// The authoritative config the store needs to stage what a settlement's
    /// snapshot could not name: the same contract [`Self::stage_candidate`]
    /// applies. The transaction checks the persisted descriptor before
    /// stamping it, so a concurrent patch forces the host to re-plan.
    pub(super) async fn turn_staging(
        &self,
        session_id: &str,
        descriptor: &Value,
    ) -> SessionsResult<TurnStaging, SessionFault<R>> {
        Ok(TurnStaging {
            kind: session_turn_kind(descriptor),
            config: Value::Object(self.turn_payload_base(session_id, descriptor).await?),
        })
    }

    /// The task kind this session's turns run under, plus the immutable
    /// snapshot every turn carries.
    ///
    pub(super) async fn turn_payload(
        &self,
        session_id: &str,
        descriptor: &Value,
        user_message_id: &str,
        assistant_message_id: &str,
        parent_message_id: Option<&str>,
    ) -> SessionsResult<Value, SessionFault<R>> {
        let mut payload = self.turn_payload_base(session_id, descriptor).await?;
        if let Some(parent) = parent_message_id {
            payload.insert("parentMessageId".into(), parent.into());
        }
        payload.insert("assistantMessageId".into(), assistant_message_id.into());
        payload.insert("userMessageId".into(), user_message_id.into());
        Ok(Value::Object(payload))
    }

    /// The config half of the staging contract: every turn of this session
    /// carries these keys, captured from the descriptor AT TASK CREATION,
    /// whoever stages it — submission, an explicit flush, a settlement's
    /// promotion, or the recovery of a half-admitted input. The refusal
    /// checks live here so no path can stage a turn the native dispatch path
    /// cannot honor.
    /// Captures immutable profile/content and current session defaults before admission.
    pub async fn snapshot_turn_configuration(
        &self,
        session_id: &str,
    ) -> SessionsResult<Value, SessionFault<R>> {
        let (_, descriptor) = self.require_session(session_id).await?;
        Ok(Value::Object(
            self.turn_payload_base(session_id, &descriptor).await?,
        ))
    }

    async fn turn_payload_base(
        &self,
        session_id: &str,
        descriptor: &Value,
    ) -> SessionsResult<Map<String, Value>, SessionFault<R>> {
        let mut payload = Map::new();
        payload.insert("chatId".into(), session_id.into());
        payload.insert("sessionDescriptor".into(), descriptor.clone());
        let profile: crate::AgentProfile = match descriptor.get("profileSnapshot") {
            Some(value) => serde_json::from_value(value.clone()).map_err(|e| {
                SessionsError::invalid_input(format!("Invalid profile snapshot: {e}"))
            })?,
            None => self
                .deps
                .profiles
                .resolve(descriptor["profile"]["id"].as_str().unwrap_or_default())
                .map_err(|error| SessionsError::InvalidInputContext(Box::new(error)))?,
        };
        if descriptor["profile"]["digest"]
            .as_str()
            .is_some_and(|d| d != profile.digest())
        {
            return Err(SessionsError::invalid_input(
                "Profile snapshot digest mismatch.",
            ));
        }
        payload.insert(
            "profileSnapshot".into(),
            serde_json::to_value(&profile).expect("profile"),
        );
        payload.insert("profileDigest".into(), profile.digest().into());
        let mut selection: crate::RunContentSelection = serde_json::from_value(
            session_content_selection(descriptor).unwrap_or_else(|| json!({})),
        )
        .map_err(|e| SessionsError::invalid_input(format!("Invalid content selection: {e}")))?;
        for skill in &profile.resources.skills {
            if !selection
                .skills
                .iter()
                .any(|selected| selected.id == skill.id)
            {
                selection.skills.push(skill.clone());
            }
        }
        if selection.template.is_some() || !selection.skills.is_empty() {
            let selection = serde_json::to_value(selection)
                .map_err(|e| SessionsError::invalid_input(e.to_string()))?;
            let snapshot = crate::run_content::freeze_run_content(
                self.store(),
                selection.clone(),
                session_id,
                self.deps.content_authority.as_ref(),
            )
            .await
            .map_err(|error| SessionsError::InvalidInputContext(Box::new(error)))?;
            payload.insert("contentSelection".into(), selection);
            payload.insert("contentSnapshot".into(), snapshot);
        }
        if descriptor.get("model").is_none()
            && let Some(model) = &profile.model
        {
            payload.insert("model".into(), model.clone().into());
        }
        if descriptor.get("reasoningEffort").is_none()
            && let Some(effort) = profile.reasoning_effort
        {
            payload.insert(
                "reasoningEffort".into(),
                serde_json::to_value(effort).expect("effort"),
            );
        }
        for (key, value) in turn_config(descriptor) {
            payload.insert(key, value);
        }
        if let Some(factory) = &self.deps.provider_factory
            && let (Some(provider), Some(model)) = (
                payload.get("providerId").and_then(Value::as_str),
                payload.get("model").and_then(Value::as_str),
            )
        {
            let cancel = agentkit_core::CancellationToken::new();
            let _cancel_on_drop = cancel.clone().drop_guard();
            let snapshot = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                factory.snapshot(provider, model, cancel),
            )
            .await
            .map_err(|_| SessionsError::invalid_input("provider_snapshot_timeout"))?
            .map_err(|error| SessionsError::InvalidInputContext(Box::new(error)))?;
            if let Some(snapshot) = snapshot {
                payload.insert("providerSnapshot".into(), snapshot);
            }
        }
        // The session-frozen native stanza is inherited verbatim by EVERY
        // turn — submission, queue flush, settlement promotion and recovery
        // all build from this base, so the dispatch decision cannot drift
        // between a live send and its queued sibling (AGENTKIT-112).
        if let Some(native) = descriptor.get("nativeExecution").and_then(Value::as_object) {
            for (key, value) in native {
                payload.insert(key.clone(), value.clone());
            }
        }
        payload.insert(
            "sessionContext".into(),
            descriptor.get("context").cloned().unwrap_or(Value::Null),
        );
        Ok(payload)
    }

    fn stage_messages(
        &self,
        session_id: &str,
        task_id: &str,
        content: &Value,
        user_message_id: &str,
        assistant_message_id: &str,
        parent_message_id: Option<&str>,
    ) -> Vec<MessageInput> {
        vec![
            MessageInput {
                id: Some(user_message_id.to_owned()),
                chat_id: session_id.to_owned(),
                role: "user".into(),
                content: content.clone(),
                parent_message_id: parent_message_id.map(str::to_owned),
                ..MessageInput::default()
            },
            MessageInput {
                id: Some(assistant_message_id.to_owned()),
                chat_id: session_id.to_owned(),
                run_id: Some(task_id.to_owned()),
                role: "assistant".into(),
                content: Value::String(String::new()),
                parent_message_id: Some(user_message_id.to_owned()),
                metadata: json!({"placeholder": true}),
                ..MessageInput::default()
            },
        ]
    }
}
