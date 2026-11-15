//! MEM-07 (roadmap 3.3): the session's core memory is the frozen S4 segment.
//! A pin during the session reaches the model as a `core_memory_changed` note
//! of the next turn context while S4 (and `memory_core_digest`) keep their
//! bytes; an explicit refreeze adopts the new block and breaks the cache once.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use agentkit_chat::*;
use agentkit_core::{ChatProvider, ChatRequest, ProviderEvent, ProviderStream, ToolRegistry};
use agentkit_engine::{SharedRuntime, SystemRuntime, TaskExecutor};
use agentkit_sessions::*;
use agentkit_store::{SqliteStore, TaskStatus};
use async_trait::async_trait;
use serde_json::json;
use tokio_util::sync::CancellationToken;

/// (system text, last user message) of every request.
type Seen = Arc<Mutex<Vec<(String, String)>>>;

struct Capturing(Seen);

#[async_trait]
impl ChatProvider for Capturing {
    fn id(&self) -> &str {
        "test"
    }
    fn stream_chat(&self, input: ChatRequest, _: CancellationToken) -> ProviderStream {
        let system = input
            .messages
            .iter()
            .filter(|m| m.role == "system")
            .map(|m| m.content.to_string())
            .collect::<String>();
        let user = input
            .messages
            .iter()
            .rev()
            .find(|m| m.role == "user")
            .map(|m| m.content.to_string())
            .unwrap_or_default();
        self.0.lock().unwrap().push((system, user));
        Box::pin(futures::stream::iter(vec![Ok(ProviderEvent::Completed {
            content: "answer".into(),
            finish_reason: "stop".into(),
            tool_calls: vec![],
        })]))
    }
}

/// The memory store, as far as the chat executor sees it.
#[derive(Default)]
struct Pins(Mutex<Vec<(String, String)>>);

impl Pins {
    fn pin(&self, id: &str, text: &str) {
        self.0.lock().unwrap().push((id.into(), text.into()));
    }

    fn unpin(&self, id: &str) {
        self.0.lock().unwrap().retain(|(pinned, _)| pinned != id);
    }
}

#[async_trait]
impl CoreMemorySource for Pins {
    async fn current(&self, _chat_id: &str) -> Result<CoreMemoryBlock, String> {
        let pins = self.0.lock().unwrap().clone();
        let entries: Vec<CoreMemoryEntry> = pins
            .iter()
            .map(|(id, text)| CoreMemoryEntry {
                id: id.clone(),
                scope: "user".into(),
                text: text.clone(),
                replaces: None,
            })
            .collect();
        let text = if entries.is_empty() {
            String::new()
        } else {
            let lines: Vec<String> = pins
                .iter()
                .map(|(_, text)| format!("[user] {text}"))
                .collect();
            format!("<memory kind=\"core\">\n{}\n</memory>", lines.join("\n"))
        };
        Ok(CoreMemoryBlock { text, entries })
    }
}

fn options(pins: &Arc<Pins>) -> ChatExecutorOptions {
    ChatExecutorOptions {
        core_memory: Some(Arc::clone(pins) as Arc<dyn CoreMemorySource>),
        ..Default::default()
    }
}

struct Session {
    host: AgentHost,
    store: SqliteStore,
    service: AgentSessionService<SqliteStore>,
    seen: Seen,
    turns: usize,
}

impl Session {
    async fn start(pins: &Arc<Pins>) -> Self {
        let store = SqliteStore::memory(64).expect("store");
        let seen: Seen = Arc::default();
        let executor: Arc<dyn TaskExecutor<SqliteStore>> = Arc::new(
            ChatExecutor::new(
                Arc::new(Capturing(Arc::clone(&seen))),
                Arc::new(ToolRegistry::new()),
            )
            .with_options(options(pins)),
        );
        let runtime: SharedRuntime = Arc::new(SystemRuntime);
        let host = AgentHost::new(
            store.clone(),
            HashMap::from([(CHAT_TURN_TASK_KIND.to_owned(), executor)]),
            Arc::clone(&runtime),
            HostOptions::default(),
        )
        .unwrap();
        let service = AgentSessionService::new(AgentSessionServiceDeps {
            store: store.clone(),
            runtime,
            plan_approval: None,
            wake: Some(host.wake_handle()),
        });
        host.set_session_flush_planner(Arc::new(service.clone()))
            .await;
        service
            .create_session(&CreateSessionInput {
                session_id: Some("s1".into()),
                title: None,
                profile_id: "coding".into(),
                profile_digest: None,
                reasoning_effort: None,
                content_selection: None,
                provider_id: None,
                model: Some("model".into()),
                workspace: None,
                context: None,
                skills: None,
                turn_kind: None,
                execution_mode: None,
                metadata: None,
            })
            .await
            .unwrap();
        Self {
            host,
            store,
            service,
            seen,
            turns: 0,
        }
    }

    /// Runs one turn to completion; returns its id.
    async fn turn(&mut self) -> String {
        self.turns += 1;
        let sent = self
            .service
            .send(
                "s1",
                &SendInput {
                    frozen_payload: None,
                    content: json!(format!("turn {}", self.turns)),
                    input_id: Some(format!("in{}", self.turns)),
                    when_busy: WhenBusy::Reject,
                    turn_id: None,
                    on_settled: None,
                },
            )
            .await
            .unwrap();
        let AgentSendResult::Turn { turn_id, .. } = sent else {
            panic!("an idle session starts the turn");
        };
        for _ in 0..20 {
            self.host.run_once().await.expect("run");
            let task = self.store.get_task(&turn_id).await.unwrap().unwrap();
            if task.status.is_terminal() {
                assert_eq!(task.status, TaskStatus::Completed);
                return turn_id;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("turn did not settle");
    }

    fn request(&self, index: usize) -> (String, String) {
        self.seen.lock().unwrap()[index].clone()
    }

    async fn core_digest(&self) -> String {
        let chat = self.store.get_chat("s1").await.unwrap().unwrap();
        read_session_prefix(&chat.metadata)
            .expect("prefix")
            .memory_core_digest
    }

    async fn cache_breaks(&self, turn_id: &str) -> Vec<String> {
        let events = self.store.events(turn_id, None, 1000).await.unwrap();
        events
            .iter()
            .filter(|e| e.event["type"] == "context.cache_break")
            .map(|e| {
                e.event["data"]["reason"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned()
            })
            .collect()
    }
}

#[tokio::test]
async fn a_pin_mid_session_is_a_note_and_s4_keeps_its_bytes_until_refreeze() {
    let pins = Arc::new(Pins::default());
    pins.pin("0193-aaaa", "Works in Rust for backend services.");
    let mut session = Session::start(&pins).await;
    session.turn().await;
    session.turn().await;
    let digest = session.core_digest().await;
    let (first, _) = session.request(0);
    assert!(
        first.contains("Works in Rust for backend services."),
        "{first}"
    );

    pins.pin("0193-bbbb", "Writes to colleagues in Slovak.");
    session.turn().await;
    let (system, user) = session.request(2);
    assert_eq!(system, first, "S4 bytes unchanged");
    assert!(!system.contains("Slovak"), "{system}");
    assert!(user.contains("core_memory_changed"), "{user}");
    assert!(
        user.contains("added: Writes to colleagues in Slovak."),
        "{user}"
    );
    assert_eq!(session.core_digest().await, digest, "S4 digest unchanged");

    // The note is delivered once.
    session.turn().await;
    assert!(!session.request(3).1.contains("core_memory_changed"));

    let adopted = refreeze_core_memory(pins.as_ref(), &session.store, "s1", 1_000)
        .await
        .unwrap();
    assert!(adopted);
    let turn_id = session.turn().await;
    let (system, user) = session.request(4);
    assert!(system.contains("Slovak"), "{system}");
    assert!(!user.contains("core_memory_changed"), "{user}");
    assert_ne!(session.core_digest().await, digest);
    assert_eq!(session.cache_breaks(&turn_id).await, vec!["refreeze"]);
    let again = refreeze_core_memory(pins.as_ref(), &session.store, "s1", 2_000)
        .await
        .unwrap();
    assert!(!again, "nothing left to adopt");
}

#[tokio::test]
async fn a_session_without_pins_has_no_s4() {
    let pins = Arc::new(Pins::default());
    let mut session = Session::start(&pins).await;
    session.turn().await;
    assert!(!session.request(0).0.contains("<memory"));
}

#[tokio::test]
async fn an_unpin_or_forget_refreezes_at_once_and_the_text_is_not_repeated() {
    let pins = Arc::new(Pins::default());
    pins.pin("0193-aaaa", "Lives in Bratislava.");
    pins.pin("0193-bbbb", "Works in Rust for backend services.");
    let mut session = Session::start(&pins).await;
    session.turn().await;
    let digest = session.core_digest().await;
    assert!(session.request(0).0.contains("Lives in Bratislava."));

    pins.unpin("0193-aaaa");
    let turn_id = session.turn().await;
    let (system, user) = session.request(1);
    assert!(
        !system.contains("Bratislava"),
        "S4 still has the text: {system}"
    );
    assert!(system.contains("Works in Rust"), "{system}");
    assert!(
        !user.contains("Bratislava"),
        "a note repeats the text: {user}"
    );
    assert!(!user.contains("core_memory_changed"), "{user}");
    assert_ne!(session.core_digest().await, digest);
    assert_eq!(session.cache_breaks(&turn_id).await, vec!["refreeze"]);

    // Nothing more to adopt on the turn after.
    let later = session.turn().await;
    assert!(session.cache_breaks(&later).await.is_empty());
}
