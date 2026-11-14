//! Session lifecycle surface of [`AgentSessionService`]: creation, state and
//! execution reads, mode switch, turn reports, fork, compaction, model and
//! configuration.

use crate::{
    AgentInputKind, AgentInputQuery, AgentInputRecord, AgentInputStatus, ChatRecord,
    ExecutionStateRead, SESSION_METADATA_KEY, STANDING_APPROVALS_METADATA_KEY, SessionBackend,
    SessionFault, SessionMutation, SessionsError, SessionsResult, execution_state_for_mode,
    inspect_execution_state, read_execution_state, with_execution,
};
use serde_json::{Map, Value, json};

use super::{
    AgentSessionService, AgentSessionState, CreateSessionInput, REASONING_EFFORTS, approval_of,
    is_valid_session_turn_kind, list_turn_tasks, read_session_descriptor, session_turn_kind,
    session_usage, summarize_turn,
};

impl<R: SessionBackend> AgentSessionService<R> {
    // -- session lifecycle --------------------------------------------------

    pub async fn create_session(
        &self,
        input: &CreateSessionInput,
    ) -> SessionsResult<AgentSessionState, SessionFault<R>> {
        let chat = self.create_session_chat(input).await?;
        self.get_state(&chat.id).await
    }

    pub async fn create_session_chat(
        &self,
        input: &CreateSessionInput,
    ) -> SessionsResult<ChatRecord, SessionFault<R>> {
        let mut metadata = match input.metadata.clone().unwrap_or_else(|| json!({})) {
            Value::Object(map) => map,
            _ => {
                return Err(SessionsError::invalid_input("metadata must be an object."));
            }
        };
        // The session-frozen native stanza arrives through caller metadata
        // under a host-namespaced key and is MOVED into the descriptor —
        // the descriptor is the only durable home, and normalization keeps
        // it verbatim (AGENTKIT-112: public normalization must not drop
        // native fields). Grammar (record, non-empty, never turnInput) is
        // enforced by the descriptor reader below.
        let native_execution = metadata.remove("agentkit.nativeExecution");
        for key in [SESSION_METADATA_KEY, STANDING_APPROVALS_METADATA_KEY] {
            if metadata.contains_key(key) {
                // Grants are written only by an answered approval; a
                // caller-supplied one would be a standing approval no human gave.
                return Err(SessionsError::invalid_input(format!(
                    "metadata must not contain {key}."
                )));
            }
        }
        if let Some(kind) = &input.turn_kind
            && !is_valid_session_turn_kind(kind)
        {
            return Err(SessionsError::invalid_input(
                "turnKind must be a dotted lowercase task kind outside the reserved chat.* / agentkit.* prefixes.",
            ));
        }
        let resolved_profile = self
            .deps
            .profiles
            .resolve(&input.profile_id)
            .map_err(|error| SessionsError::InvalidInputContext(Box::new(error)))?;
        let digest = resolved_profile.digest();
        if input.profile_digest.as_ref().is_some_and(|d| d != &digest) {
            return Err(SessionsError::invalid_input(
                "Profile digest does not match resolved profile.",
            ));
        }
        let mut raw = Map::new();
        raw.insert(
            "profileSnapshot".into(),
            serde_json::to_value(&resolved_profile).expect("profile"),
        );
        raw.insert("version".into(), Value::from(1));
        let mut profile = Map::new();
        profile.insert("id".into(), input.profile_id.clone().into());
        profile.insert("digest".into(), digest.into());
        raw.insert("profile".into(), Value::Object(profile));
        for (key, value) in [
            (
                "providerId",
                &input
                    .provider_id
                    .clone()
                    .or(self.deps.default_provider_id.clone()),
            ),
            (
                "model",
                &input
                    .model
                    .clone()
                    .or(resolved_profile.model.clone())
                    .or(self.deps.default_model.clone()),
            ),
        ] {
            if let Some(value) = value {
                raw.insert(key.into(), value.clone().into());
            }
        }
        if let Some(effort) = input.reasoning_effort.or(resolved_profile.reasoning_effort) {
            raw.insert(
                "reasoningEffort".into(),
                serde_json::to_value(effort).expect("effort"),
            );
        }
        if let Some(selection) = &input.content_selection {
            raw.insert(
                "contentSelection".into(),
                serde_json::to_value(selection).expect("selection"),
            );
        }
        raw.insert("createdAt".into(), self.now_iso().into());
        for (key, value) in [
            ("workspace", &input.workspace),
            ("context", &input.context),
            ("skills", &input.skills.clone().map(Value::Array)),
            ("turnKind", &input.turn_kind.clone().map(Value::String)),
        ] {
            if let Some(value) = value {
                raw.insert(key.into(), value.clone());
            }
        }
        if let Some(native) = native_execution {
            raw.insert("nativeExecution".into(), native);
        }
        metadata.insert(SESSION_METADATA_KEY.into(), Value::Object(raw));
        if read_session_descriptor(&Value::Object(metadata.clone())).is_none() {
            // Malformed-present values (a scalar workspace, a string
            // windowTokens, a Skills entry without revision, an illegal
            // turnKind, …) invalidate the descriptor exactly like a bad
            // `profile.id` — they are never silently dropped.
            return Err(SessionsError::invalid_input(
                "profile.id must be a non-empty string and workspace/context/skills/turnKind \
                 must match the session descriptor grammar.",
            ));
        }
        let initial_mode = input
            .execution_mode
            .clone()
            .unwrap_or(resolved_profile.permission_mode.clone());
        let state = match &Some(initial_mode) {
            Some(mode) => {
                // Validate the mode first so a bad one cannot half-create
                // the session.
                let mode = mode.clone();
                let updated_at = self.now_iso();
                Some(
                    execution_state_for_mode(&mode, &updated_at, None)
                        .ok_or_else(|| SessionsError::invalid_input("mode must be non-empty."))?,
                )
            }
            None => None,
        };
        let metadata = with_execution(&Value::Object(metadata), state);
        let session_id = input
            .session_id
            .clone()
            .unwrap_or_else(|| self.mint("chat_"));
        let chat = self
            .store()
            .create_chat(&session_id, input.title.clone(), metadata, self.now_ms())
            .await?;
        Ok(chat)
    }

    pub async fn get_state(
        &self,
        session_id: &str,
    ) -> SessionsResult<AgentSessionState, SessionFault<R>> {
        let (chat, descriptor) = self.require_session(session_id).await?;
        let active = self
            .active_turn(session_id, &session_turn_kind(&descriptor))
            .await?;
        let records = self
            .store()
            .list_inputs(AgentInputQuery {
                session_id: session_id.to_owned(),
                kinds: None,
                statuses: Some(vec![AgentInputStatus::Pending]),
                target_task_id: None,
            })
            .await?;
        let waiting = records
            .iter()
            .find(|record| record.kind == AgentInputKind::HumanRequest)
            .cloned();
        let pending: Vec<AgentInputRecord> = records
            .into_iter()
            .filter(|record| record.kind != AgentInputKind::HumanRequest)
            .collect();
        let execution = read_execution_state(&chat.metadata);
        let waiting_plan = waiting.as_ref().and_then(|waiting| {
            self.plan_request_of(waiting).map(|plan| {
                let mut out = plan.as_object().cloned().unwrap_or_default();
                out.insert("requestId".into(), waiting.input_id.clone().into());
                out.insert(
                    "toolName".into(),
                    waiting.content["toolName"]
                        .as_str()
                        .unwrap_or_default()
                        .into(),
                );
                Value::Object(out)
            })
        });
        Ok(AgentSessionState {
            session_id: session_id.to_owned(),
            descriptor,
            active_turn: active.map(|task| {
                json!({"turnId": task.task_id, "status": serde_json::to_value(task.status).unwrap_or(Value::Null)})
            }),
            pending,
            waiting: waiting.clone(),
            waiting_approval: waiting.as_ref().and_then(approval_of),
            waiting_plan,
            execution,
        })
    }

    /// The session's durable permission state, when one was ever set.
    /// Corrupt state is an ERROR, never absence: a caller falling back to
    /// its configured permissions would silently replace the human's
    /// recorded authority with whatever the process happens to allow.
    pub async fn get_execution(
        &self,
        session_id: &str,
    ) -> SessionsResult<Option<Value>, SessionFault<R>> {
        let (chat, _) = self.require_session(session_id).await?;
        match inspect_execution_state(&chat.metadata) {
            ExecutionStateRead::Absent => Ok(None),
            ExecutionStateRead::Valid(state) => Ok(Some(state)),
            ExecutionStateRead::Malformed => Err(SessionsError::invalid_input(
                "The session's persisted execution state is malformed; refusing to fall back \
                 to configured permissions. Repair the record before granting access.",
            )),
        }
    }

    /// An explicit, persisted mode switch by the HOST or the user — never by
    /// the model. Entering `plan` drops any grant, so a session that was
    /// cleared to edit cannot read its way back into write access without a
    /// new approval.
    pub async fn set_execution_mode(
        &self,
        session_id: &str,
        mode: &str,
    ) -> SessionsResult<Value, SessionFault<R>> {
        let chat = self
            .store()
            .patch_session(
                session_id,
                SessionMutation::ExecutionMode {
                    mode: mode.to_owned(),
                },
                self.now_ms(),
            )
            .await?;
        // The mutation refuses an empty mode before it writes, so the state is
        // always there once the commit lands.
        read_execution_state(&chat.metadata)
            .ok_or_else(|| SessionsError::invalid_input("mode must be non-empty."))
    }

    // -- reports ---------------------------------------------------------------

    pub async fn list_turns(
        &self,
        session_id: &str,
    ) -> SessionsResult<Vec<Value>, SessionFault<R>> {
        let (_, descriptor) = self.require_session(session_id).await?;
        Ok(
            list_turn_tasks(self.store(), session_id, &session_turn_kind(&descriptor))
                .await?
                .iter()
                .map(summarize_turn)
                .collect(),
        )
    }

    pub async fn get_usage(&self, session_id: &str) -> SessionsResult<Value, SessionFault<R>> {
        let (_, descriptor) = self.require_session(session_id).await?;
        session_usage(self.store(), session_id, &descriptor).await
    }

    /// A JSON-serializable history: active-path messages, turns, inputs.
    pub async fn export_session(&self, session_id: &str) -> SessionsResult<Value, SessionFault<R>> {
        let (chat, descriptor) = self.require_session(session_id).await?;
        super::report::export_session(
            self.store(),
            session_id,
            &descriptor,
            &chat.metadata,
            &self.now_iso(),
        )
        .await
    }

    // -- deferred compaction, queue maintenance, configuration -------------------

    /// Request deferred compaction at the next session turn (ADR 0017 part 2).
    pub async fn compact(&self, session_id: &str) -> SessionsResult<(), SessionFault<R>> {
        let (chat, descriptor) = self.require_session(session_id).await?;
        self.store()
            .request_compact(
                session_id.into(),
                session_turn_kind(&descriptor),
                chat.metadata[SESSION_METADATA_KEY].clone(),
                self.now_ms(),
            )
            .await?;
        Ok(())
    }

    /// Pending inputs and the provider continuation are NOT copied.
    pub async fn fork(
        &self,
        session_id: &str,
        from_message_id: &str,
    ) -> SessionsResult<AgentSessionState, SessionFault<R>> {
        let (_, _) = self.require_session(session_id).await?;
        let forked_id = self.mint("chat_");
        self.store()
            .fork_chat(session_id, from_message_id, &forked_id, self.now_ms())
            .await?;
        let session_id = session_id.to_owned();
        let from = from_message_id.to_owned();
        self.store()
            .patch_session(
                &forked_id,
                SessionMutation::ForkDescriptor {
                    forked_from: json!({"sessionId": session_id, "messageId": from}),
                },
                self.now_ms(),
            )
            .await?;
        self.get_state(&forked_id).await
    }

    /// Future turns only; an active task keeps the snapshot (model,
    /// provider) it was submitted with, and a patch never touches turns
    /// already queued.
    pub async fn set_model(
        &self,
        session_id: &str,
        provider_id: Option<&str>,
        model: &str,
        reasoning_effort: Option<Option<&str>>,
    ) -> SessionsResult<AgentSessionState, SessionFault<R>> {
        if model.is_empty() {
            return Err(SessionsError::invalid_input(
                "model must be a non-empty string.",
            ));
        }
        if let Some(Some(effort)) = &reasoning_effort
            && !REASONING_EFFORTS.contains(effort)
        {
            return Err(SessionsError::invalid_input(format!(
                "reasoningEffort must be one of {}.",
                REASONING_EFFORTS.join(", ")
            )));
        }
        let model = model.to_owned();
        let provider_id = provider_id.map(str::to_owned);
        let effort = reasoning_effort.map(|value| value.map(str::to_owned));
        // Three-state patch, matching TS setModel ("Omit to keep the current
        // effort; `null` falls back to the profile's."): omitted preserves,
        // explicit null clears, a value replaces. `providerId` has no
        // wire-level clear — TS replaces only — so `Option<&str>` carries its
        // full patch contract.
        let mut replace = Map::new();
        replace.insert("model".into(), model.into());
        let mut remove = Vec::new();
        match &effort {
            None => {}
            Some(None) => remove.push("reasoningEffort".to_owned()),
            Some(Some(effort)) => {
                replace.insert("reasoningEffort".into(), effort.clone().into());
            }
        }
        if let Some(provider_id) = &provider_id {
            replace.insert("providerId".into(), provider_id.clone().into());
        }
        self.store()
            .patch_session(
                session_id,
                SessionMutation::PatchDescriptor {
                    replace,
                    merge: Map::new(),
                    remove,
                    invalid_message: "descriptor invalid after model change".to_owned(),
                },
                self.now_ms(),
            )
            .await?;
        self.get_state(session_id).await
    }

    /// Replace the session's Skills, context options and/or workspace trust
    /// flag (the root never changes). Future turns only; an active task keeps
    /// what it was submitted with.
    pub async fn configure(
        &self,
        session_id: &str,
        skills: Option<Vec<Value>>,
        context: Option<Value>,
        workspace_trusted: Option<bool>,
    ) -> SessionsResult<AgentSessionState, SessionFault<R>> {
        if workspace_trusted.is_some() {
            let (_, descriptor) = self.require_session(session_id).await?;
            if descriptor.get("workspace").is_none() {
                return Err(SessionsError::invalid_input(
                    "workspaceTrusted needs a session with a workspace.",
                ));
            }
        }
        let skills = skills.map(Value::Array);
        let mut replace = Map::new();
        if let Some(skills) = &skills {
            replace.insert("skills".into(), skills.clone());
        }
        if let Some(context) = &context {
            replace.insert("context".into(), context.clone());
        }
        // Trust rides inside the workspace object: extended, never replaced,
        // and a descriptor without a workspace object keeps what it had.
        let mut merge = Map::new();
        if let Some(trusted) = workspace_trusted {
            merge.insert("workspace".into(), json!({"trusted": trusted}));
        }
        self.store()
            .patch_session(
                session_id,
                SessionMutation::PatchDescriptor {
                    replace,
                    merge,
                    remove: vec![],
                    invalid_message: "skills must be {id, revision} strings and context valid."
                        .to_owned(),
                },
                self.now_ms(),
            )
            .await?;
        self.get_state(session_id).await
    }
}
