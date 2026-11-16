//! W2 auto-capture (roadmap 3.4, spec "W2: auto-capture"): after an owner
//! turn completes, a background task of kind `agentkit.memory.extract` is
//! queued at low priority in its own scope, so it never delays the reply.
//! The executor reads the owner's eligible typed text of that turn (and
//! nothing else), makes one tool-less call on the configured background
//! provider, and runs what comes back through the write gate in
//! [`agentkit_memory`]. Each capture is announced as a `memory.captured`
//! event so a host can show the chip with Undo.

use std::sync::Arc;

use agentkit_core::{
    CancellationToken, ChatMessage, ChatProvider, ChatRequest, ProviderEvent, Runtime,
    SUBAGENT_METADATA_KEY,
};
use agentkit_engine::{
    EngineError, EngineResult, ExecutionContext, ObserverError, SubmitTask, TaskExecutor,
    TaskRecord, TaskSettledObserver, TaskStatus,
};
use agentkit_memory::{
    CaptureConfig, CaptureOutcome, CaptureTurn, MemoryService, Rejection, Scope, Timestamp,
    extraction_prompt, parse_candidates, restrict_updates,
};
use agentkit_sessions::read_session_descriptor;
use agentkit_store::{SqliteStore, StoreError};
use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{Value, json};

use super::memory::{MEMORY_SESSION_METADATA_KEY, REMOVED, owner_submitted, typed_text};

/// The task kind of one extraction.
pub const MEMORY_EXTRACT_TASK_KIND: &str = "agentkit.memory.extract";
/// Chat turns run at priority 0; an extraction yields to every turn.
const EXTRACT_PRIORITY: i32 = -10;
/// The extractor answers with at most three short records.
const EXTRACT_MAX_OUTPUT_TOKENS: u64 = 1024;
/// What stands in the extractor's input for a stretch that was not typed.
const OMITTED: &str = "[omitted]";

/// The scopes whose auto-capture is on for a chat (spec: a setting per
/// scope, default on for user and project). The setting is the chat
/// metadata `agentkit.memory.autoCapture`: `{"user": bool, "project": bool,
/// "agent": bool}`. The user scope is always the owner's; the project scope
/// exists when the host named a project; the agent scope exists for a
/// session profile and is off unless set.
pub(crate) fn capture_scopes(metadata: &Value) -> Vec<Scope> {
    let memory = &metadata[MEMORY_SESSION_METADATA_KEY];
    let on = |key: &str, default: bool| memory["autoCapture"][key].as_bool().unwrap_or(default);
    let mut scopes = Vec::new();
    if on("user", true) {
        scopes.push(Scope::user());
    }
    if let Some(project) = memory["projectId"].as_str().filter(|id| !id.is_empty())
        && on("project", true)
    {
        scopes.push(Scope::project(project));
    }
    if let Some(agent) = read_session_descriptor(metadata)
        .as_ref()
        .and_then(|descriptor| descriptor["profile"]["id"].as_str().map(str::to_owned))
        && on("agent", false)
    {
        scopes.push(Scope::agent(agent));
    }
    scopes
}

/// Queues an extraction when an owner turn completes. Best effort and
/// idempotent: the task id is derived from the message id, so a replayed
/// observation (or a restart sweep) queues nothing new.
pub(crate) struct MemoryCaptureObserver {
    store: SqliteStore,
    clock: Arc<dyn Runtime>,
}

#[async_trait]
impl TaskSettledObserver for MemoryCaptureObserver {
    async fn observe_settled(&self, task: &TaskRecord) -> Result<(), ObserverError> {
        if task.kind == MEMORY_EXTRACT_TASK_KIND || task.status != TaskStatus::Completed {
            return Ok(());
        }
        // A session turn: its scope is the chat and its payload names the
        // owner's message. Anything else is not an owner turn.
        let (Some(chat_id), Some(message_id)) = (
            task.payload["chatId"].as_str(),
            task.payload["userMessageId"].as_str(),
        ) else {
            return Ok(());
        };
        if task.scope_id != chat_id {
            return Ok(());
        }
        let Some(chat) = self.store.get_chat(chat_id).await? else {
            return Ok(());
        };
        // A child agent's chat has no owner in it.
        if chat.metadata.get(SUBAGENT_METADATA_KEY).is_some()
            || capture_scopes(&chat.metadata).is_empty()
        {
            return Ok(());
        }
        let queued = self
            .store
            .submit(SubmitTask {
                task_id: format!("memory-extract:{message_id}"),
                kind: MEMORY_EXTRACT_TASK_KIND.to_owned(),
                scope_id: format!("memory-extract:{chat_id}"),
                payload: json!({
                    "chatId": chat_id,
                    "userMessageId": message_id,
                    "turnTaskId": task.task_id,
                }),
                priority: EXTRACT_PRIORITY,
                now: self.clock.now_ms(),
                available_at: None,
                depends_on: vec![],
                parent_task_id: None,
                // Not `exclusive_scope`: that refuses a second submit while
                // one is open. Extractions of a chat share a scope instead,
                // and the claim never starts a task while another of its
                // scope runs, so they go one at a time and the gate's cap
                // checks see the records of the one before.
                exclusive_scope: false,
            })
            .await;
        match queued {
            Ok(_) | Err(StoreError::Duplicate(_)) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

/// Runs one extraction: eligible text in, gated records out.
pub(crate) struct MemoryExtractExecutor {
    service: Arc<MemoryService>,
    provider: Arc<dyn ChatProvider>,
    model: String,
    config: CaptureConfig,
}

impl MemoryExtractExecutor {
    /// The owner's typed words of the message, ready for the extractor, and
    /// the stored content they came from. `None` when the turn is not an
    /// owner's or holds nothing typed.
    async fn owner_input(
        store: &SqliteStore,
        chat_id: &str,
        message_id: &str,
    ) -> Result<Option<(Vec<Scope>, String, String)>, StoreError> {
        let Some(chat) = store.get_chat(chat_id).await? else {
            return Ok(None);
        };
        if chat.metadata.get(SUBAGENT_METADATA_KEY).is_some() {
            return Ok(None);
        }
        let scopes = capture_scopes(&chat.metadata);
        let Some(message) = store.get_message(message_id).await? else {
            return Ok(None);
        };
        if scopes.is_empty()
            || message.chat_id != chat_id
            || message.role != "user"
            || message.metadata.get(SUBAGENT_METADATA_KEY).is_some()
            || !owner_submitted(&message.metadata)
        {
            return Ok(None);
        }
        let eligible = typed_text(&message.content);
        if eligible.replace(REMOVED, "").trim().is_empty() {
            return Ok(None);
        }
        let content = match &message.content {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        };
        Ok(Some((scopes, eligible, content)))
    }

    /// The extractor's one call: the fixed instruction and the owner's
    /// typed words, no tools. The error is the provider's failure.
    async fn extract(
        &self,
        run_id: &str,
        system: &str,
        eligible: &str,
        cancellation: CancellationToken,
    ) -> Result<String, String> {
        let mut request = ChatRequest {
            run_id: run_id.to_owned(),
            model: self.model.clone(),
            messages: vec![
                ChatMessage::text("system", system),
                ChatMessage::text("user", &eligible.replace(REMOVED, OMITTED)),
            ],
            tools: vec![],
            temperature: Some(0.0),
            max_output_tokens: Some(EXTRACT_MAX_OUTPUT_TOKENS),
            ..Default::default()
        };
        self.provider.apply_request_defaults(&mut request);
        let mut stream = self.provider.stream_chat(request, cancellation);
        let mut streamed = String::new();
        while let Some(event) = stream.next().await {
            match event.map_err(|error| error.to_string())? {
                ProviderEvent::Delta(delta) => streamed.push_str(&delta),
                ProviderEvent::Completed { content, .. } => {
                    return Ok(if content.is_empty() {
                        streamed
                    } else {
                        content
                    });
                }
                _ => {}
            }
        }
        Ok(streamed)
    }
}

async fn warn(
    context: &ExecutionContext<SqliteStore>,
    code: &str,
    message: String,
) -> EngineResult<(), StoreError> {
    context
        .emit("run.warning", json!({"code": code, "message": message}))
        .await
}

#[async_trait]
impl TaskExecutor<SqliteStore> for MemoryExtractExecutor {
    async fn execute(
        &self,
        context: ExecutionContext<SqliteStore>,
    ) -> EngineResult<(), StoreError> {
        let (Some(chat_id), Some(message_id)) = (
            context.task.payload["chatId"].as_str(),
            context.task.payload["userMessageId"].as_str(),
        ) else {
            return Ok(());
        };
        context.check_cancelled()?;
        let input = Self::owner_input(&context.store, chat_id, message_id)
            .await
            .map_err(EngineError::Store)?;
        let Some((scopes, eligible, content)) = input else {
            return Ok(());
        };
        let task_id = context.task.task_id.clone();
        // The scope's current records the extractor may name in `updates`.
        // Best effort: without them every candidate is a plain capture.
        let known = self
            .service
            .known_records(&scopes, &eligible.replace(REMOVED, " "))
            .await
            .unwrap_or_default();
        let system = extraction_prompt(&known);
        let raw = match self
            .extract(&task_id, &system, &eligible, context.cancellation.clone())
            .await
        {
            Ok(raw) => raw,
            Err(error) => {
                return warn(
                    &context,
                    "memory_extract_failed",
                    format!("The memory extraction call failed: {error}"),
                )
                .await;
            }
        };
        let mut candidates = match parse_candidates(&raw) {
            Ok(candidates) => candidates,
            Err(error) => {
                return warn(
                    &context,
                    "memory_extract_malformed",
                    format!("Nothing was captured: {error}"),
                )
                .await;
            }
        };
        restrict_updates(&mut candidates, &known);
        context.check_cancelled()?;
        let turn = CaptureTurn {
            chat_id,
            message_id,
            eligible_text: &eligible,
            content: &content,
            task: Some(&task_id),
            scopes: &scopes,
        };
        let now = Timestamp(context.runtime.now_ms());
        // One candidate at a time: each is announced as soon as it commits,
        // and a later failure leaves the earlier records and events alone.
        let turn_task = context.task.payload["turnTaskId"].as_str();
        for candidate in candidates {
            let outcomes = match self
                .service
                .capture(&turn, vec![candidate], &self.config, now)
                .await
            {
                Ok(outcomes) => outcomes,
                Err(error) => {
                    return warn(
                        &context,
                        "memory_capture_failed",
                        format!("Memory capture failed: {error}"),
                    )
                    .await;
                }
            };
            for outcome in outcomes {
                match outcome {
                    CaptureOutcome::Captured {
                        id,
                        text,
                        superseded,
                    } => {
                        let data = json!({
                            "recordId": id.to_string(),
                            "text": text,
                            "chatId": chat_id,
                            "messageId": message_id,
                            "replaces": superseded.map(|old| old.to_string()),
                        });
                        context.emit("memory.captured", data).await?;
                        // The owner's chat shows the chip from its own
                        // stream: the turn's. Ids only (the store takes no
                        // content on a stream it does not own); the host
                        // reads the record. A failure here loses the
                        // mirror, not the capture.
                        if let Some(turn_task) = turn_task {
                            let mirror = json!({
                                "recordId": id.to_string(),
                                "chatId": chat_id,
                                "messageId": message_id,
                                "replaces": superseded.map(|old| old.to_string()),
                            });
                            let _ = context
                                .store
                                .append_system_event(turn_task, "memory.captured", mirror, now.0)
                                .await;
                        }
                    }
                    CaptureOutcome::Rejected(Rejection::ProposalLimit) => {
                        warn(
                            &context,
                            "memory_proposal_limit",
                            "A memory proposal was dropped: the scope's daily limit is reached."
                                .to_owned(),
                        )
                        .await?;
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }
}

/// The observer and the executor, built when the runtime has both a memory
/// service and a background provider.
pub(crate) struct MemoryCaptureLane {
    pub(crate) observer: Arc<dyn TaskSettledObserver>,
    pub(crate) executor: Arc<dyn TaskExecutor<SqliteStore>>,
}

impl MemoryCaptureLane {
    pub(crate) fn new(
        service: Arc<MemoryService>,
        store: SqliteStore,
        clock: Arc<dyn Runtime>,
        provider: Arc<dyn ChatProvider>,
        model: String,
    ) -> Self {
        Self {
            observer: Arc::new(MemoryCaptureObserver { store, clock }),
            executor: Arc::new(MemoryExtractExecutor {
                service,
                provider,
                model,
                config: CaptureConfig::default(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentkit_engine::MessageInput;

    #[test]
    fn auto_capture_is_per_scope_user_and_project_on_agent_off() {
        let none = json!({});
        assert_eq!(capture_scopes(&none), vec![Scope::user()]);
        let project = json!({"agentkit.memory": {"projectId": "p1"}});
        assert_eq!(
            capture_scopes(&project),
            vec![Scope::user(), Scope::project("p1")]
        );
        let user_off =
            json!({"agentkit.memory": {"projectId": "p1", "autoCapture": {"user": false}}});
        assert_eq!(capture_scopes(&user_off), vec![Scope::project("p1")]);
        let all_off = json!({"agentkit.memory": {"projectId": "p1", "autoCapture": {"user": false, "project": false}}});
        assert!(capture_scopes(&all_off).is_empty());
        // The agent scope exists only with a session profile, and is opt-in.
        let session = json!({
            "agentkit.session": {"version": 1, "profile": {"id": "coder"}, "createdAt": "2026-01-01T00:00:00Z"},
            "agentkit.memory": {"autoCapture": {"agent": true}},
        });
        assert_eq!(
            capture_scopes(&session),
            vec![Scope::user(), Scope::agent("coder")]
        );
        let session_default = json!({"agentkit.session": {"version": 1, "profile": {"id": "coder"}, "createdAt": "2026-01-01T00:00:00Z"}});
        assert_eq!(capture_scopes(&session_default), vec![Scope::user()]);
    }

    async fn seeded_chat(store: &SqliteStore, chat: &str, metadata: Value, text: &str) -> String {
        store.create_chat(chat, None, metadata, 1).await.unwrap();
        let user = format!("{chat}-user");
        store
            .append_message(
                MessageInput {
                    id: Some(user.clone()),
                    chat_id: chat.into(),
                    role: "user".into(),
                    content: json!(text),
                    ..Default::default()
                },
                None,
                2,
            )
            .await
            .unwrap();
        user
    }

    /// A turn task as the host settles it: scope = chat, payload names the
    /// owner's message.
    async fn turn_task(
        store: &SqliteStore,
        chat: &str,
        message: &str,
        status: TaskStatus,
    ) -> TaskRecord {
        let mut task = store
            .submit(SubmitTask {
                task_id: format!("turn-{chat}"),
                kind: "chat.turn".into(),
                scope_id: chat.into(),
                payload: json!({"chatId": chat, "userMessageId": message}),
                priority: 0,
                now: 1,
                available_at: None,
                depends_on: vec![],
                parent_task_id: None,
                exclusive_scope: true,
            })
            .await
            .unwrap();
        task.status = status;
        task
    }

    #[tokio::test]
    async fn mem_05_a_completed_owner_turn_queues_one_idempotent_low_priority_extraction() {
        let store = SqliteStore::memory(32).expect("store");
        let user = seeded_chat(&store, "chat", json!({}), "I work in Rust.").await;
        let observer = MemoryCaptureObserver {
            store: store.clone(),
            clock: Arc::new(agentkit_core::SystemRuntime),
        };
        let task = turn_task(&store, "chat", &user, TaskStatus::Completed).await;
        observer.observe_settled(&task).await.unwrap();
        // The same observation twice queues nothing new.
        observer.observe_settled(&task).await.unwrap();
        let queued = store
            .get_task(&format!("memory-extract:{user}"))
            .await
            .unwrap()
            .expect("queued");
        assert_eq!(queued.kind, MEMORY_EXTRACT_TASK_KIND);
        assert_eq!(queued.scope_id, "memory-extract:chat");
        assert!(queued.priority < 0, "low priority");
        assert_eq!(queued.payload["userMessageId"], json!(user));
    }

    #[tokio::test]
    async fn mem_05_no_task_for_a_failed_turn_a_child_chat_or_a_setting_that_is_off() {
        let store = SqliteStore::memory(32).expect("store");
        let observer = MemoryCaptureObserver {
            store: store.clone(),
            clock: Arc::new(agentkit_core::SystemRuntime),
        };
        let cases = [
            ("failed", json!({}), TaskStatus::Failed),
            (
                "child",
                json!({SUBAGENT_METADATA_KEY: {"role": "reviewer"}}),
                TaskStatus::Completed,
            ),
            (
                "off",
                json!({"agentkit.memory": {"autoCapture": {"user": false}}}),
                TaskStatus::Completed,
            ),
        ];
        for (chat, metadata, status) in cases {
            let user = seeded_chat(&store, chat, metadata, "I work in Rust.").await;
            let task = turn_task(&store, chat, &user, status).await;
            observer.observe_settled(&task).await.unwrap();
            assert!(
                store
                    .get_task(&format!("memory-extract:{user}"))
                    .await
                    .unwrap()
                    .is_none(),
                "{chat}"
            );
        }
    }

    #[tokio::test]
    async fn mem_05_extractions_of_one_chat_run_one_at_a_time() {
        let store = SqliteStore::memory(32).expect("store");
        let observer = MemoryCaptureObserver {
            store: store.clone(),
            clock: Arc::new(agentkit_core::SystemRuntime),
        };
        store.create_chat("chat", None, json!({}), 1).await.unwrap();
        let base = turn_task(&store, "chat", "user-0", TaskStatus::Completed).await;
        for n in 0..2 {
            let user = format!("user-{n}");
            store
                .append_message(
                    MessageInput {
                        id: Some(user.clone()),
                        chat_id: "chat".into(),
                        role: "user".into(),
                        content: json!("I work in Rust."),
                        ..Default::default()
                    },
                    None,
                    2,
                )
                .await
                .unwrap();
            let mut task = base.clone();
            task.task_id = format!("turn-{n}");
            task.payload = json!({"chatId": "chat", "userMessageId": user});
            observer.observe_settled(&task).await.unwrap();
        }
        let claim = || agentkit_store::ClaimInput {
            owner_id: "worker".into(),
            // The observer queues with the wall clock, so the claim must be
            // later than any real instant.
            now: 4_000_000_000_000,
            ttl_ms: 60_000,
            kinds: vec![MEMORY_EXTRACT_TASK_KIND.into()],
            busy_scopes: vec![],
            max_attempts: 3,
        };
        assert!(store.claim(claim()).await.unwrap().is_some());
        // Same scope: the second waits for the first to settle.
        assert!(store.claim(claim()).await.unwrap().is_none());
    }
}
