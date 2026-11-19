//! CE-06 (roadmap 1.4): the session's project rules are read once into the
//! frozen S3 segment. An edit reaches the model as a `project_rules_changed`
//! note of the next turn context while S3 (and `rules_digest`) keep their
//! bytes; an explicit refreeze, or notes past 8 KiB, adopt the new rules.
#![cfg(unix)]

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
use tempfile::TempDir;
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

fn options() -> AgentExecutorOptions {
    AgentExecutorOptions {
        project_instructions: Some(Arc::new(FileProjectInstructions)),
        ..Default::default()
    }
}

struct Session {
    host: AgentHost,
    service: AgentSessionService<SqliteStore>,
    store: SqliteStore,
    seen: Seen,
    turns: usize,
}

impl Session {
    async fn start(dir: &TempDir) -> Self {
        Self::start_with(dir, true).await
    }

    async fn start_with(dir: &TempDir, trusted: bool) -> Self {
        let store = SqliteStore::memory(64).expect("store");
        let seen: Seen = Arc::default();
        let executor: Arc<dyn TaskExecutor<SqliteStore>> = Arc::new(
            AgentExecutor::new(
                Arc::new(Capturing(Arc::clone(&seen))),
                Arc::new(ToolRegistry::new()),
            )
            .with_options(options()),
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
                workspace: Some(
                    json!({ "root": dir.path().display().to_string(), "trusted": trusted }),
                ),
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
            service,
            store,
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

    async fn rules_digest(&self) -> String {
        let chat = self.store.get_chat("s1").await.unwrap().unwrap();
        read_session_prefix(&chat.metadata)
            .expect("prefix")
            .rules_digest
    }

    async fn warnings(&self, turn_id: &str) -> Vec<String> {
        let events = self.store.events(turn_id, None, 1000).await.unwrap();
        events
            .iter()
            .filter(|e| e.event["type"] == "run.warning")
            .map(|e| {
                e.event["data"]["code"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned()
            })
            .collect()
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

fn rules(ten: &str) -> String {
    (1..=20)
        .map(|n| {
            if n == 10 {
                format!("{ten}\n")
            } else {
                format!("rule {n}\n")
            }
        })
        .collect()
}

fn write_agents(dir: &TempDir, text: &str) {
    std::fs::write(dir.path().join("AGENTS.md"), text).expect("write AGENTS.md");
}

async fn through_turn_five_then_edit(dir: &TempDir) -> Session {
    write_agents(dir, &rules("rule 10"));
    let mut session = Session::start(dir).await;
    for _ in 0..5 {
        session.turn().await;
    }
    write_agents(dir, &rules("rule ten edited"));
    session
}

#[tokio::test]
async fn an_edit_at_turn_five_is_a_diff_note_on_turn_six_and_s3_keeps_its_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = through_turn_five_then_edit(&dir).await;
    let digest = session.rules_digest().await;
    session.turn().await;
    for turn in 0..5 {
        let (system, user) = session.request(turn);
        assert!(system.contains("rule 10"), "S3 carries the rules: {system}");
        assert!(
            !user.contains("project_rules_changed"),
            "turn {turn}: {user}"
        );
    }
    let (system, user) = session.request(5);
    assert_eq!(system, session.request(0).0, "S3 bytes unchanged");
    assert!(!system.contains("ten edited"), "{system}");
    assert!(user.contains("project_rules_changed"), "{user}");
    assert!(user.contains("-rule 10"), "{user}");
    assert!(user.contains("+rule ten edited"), "{user}");
    assert_eq!(session.rules_digest().await, digest, "S3 digest unchanged");
    // The note is delivered once.
    let turn_id = session.turn().await;
    assert!(!session.request(6).1.contains("project_rules_changed"));
    assert_eq!(session.rules_digest().await, digest);
    assert!(session.cache_breaks(&turn_id).await.is_empty());
}

#[tokio::test]
async fn an_explicit_refreeze_adopts_the_edit_and_breaks_the_cache_once() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = through_turn_five_then_edit(&dir).await;
    let digest = session.rules_digest().await;
    session.turn().await;
    let adopted = refreeze_project_rules(&options(), &session.store, "s1", 1_000)
        .await
        .unwrap();
    assert!(adopted);
    let turn_id = session.turn().await;
    let (system, user) = session.request(6);
    assert!(system.contains("rule ten edited"), "{system}");
    assert!(!user.contains("project_rules_changed"), "{user}");
    assert_ne!(session.rules_digest().await, digest);
    assert_eq!(session.cache_breaks(&turn_id).await, vec!["refreeze"]);
    let again = refreeze_project_rules(&options(), &session.store, "s1", 2_000)
        .await
        .unwrap();
    assert!(!again, "nothing left to adopt");
}

#[tokio::test]
async fn notes_past_eight_kib_refreeze_instead_of_queueing() {
    let dir = tempfile::tempdir().unwrap();
    let before: String = (0..600).map(|n| format!("old rule number {n}\n")).collect();
    write_agents(&dir, &before);
    let mut session = Session::start(&dir).await;
    for _ in 0..2 {
        session.turn().await;
    }
    let digest = session.rules_digest().await;
    let after: String = (0..600).map(|n| format!("new rule number {n}\n")).collect();
    assert!(after.len() > REFREEZE_DIFF_BYTES);
    write_agents(&dir, &after);
    let turn_id = session.turn().await;
    let (system, user) = session.request(2);
    assert!(system.contains("new rule number 599"), "S3 re-frozen");
    assert!(
        !user.contains("project_rules_changed"),
        "no note: it is S3 now"
    );
    assert_ne!(session.rules_digest().await, digest);
    assert_eq!(session.cache_breaks(&turn_id).await, vec!["refreeze"]);
}

#[tokio::test]
async fn a_symlink_swapped_in_later_keeps_the_text_and_warns_once() {
    let dir = tempfile::tempdir().unwrap();
    write_agents(&dir, &rules("rule 10"));
    let mut session = Session::start(&dir).await;
    session.turn().await;
    session.turn().await;
    let digest = session.rules_digest().await;
    let elsewhere = dir.path().join("elsewhere.md");
    std::fs::write(&elsewhere, "secret text").unwrap();
    std::fs::remove_file(dir.path().join("AGENTS.md")).unwrap();
    std::os::unix::fs::symlink(&elsewhere, dir.path().join("AGENTS.md")).unwrap();
    let third = session.turn().await;
    assert_eq!(
        session.warnings(&third).await,
        vec!["project_instructions_unavailable"]
    );
    let fourth = session.turn().await;
    assert!(
        session.warnings(&fourth).await.is_empty(),
        "once per change of state"
    );
    for turn in 0..4 {
        let (system, user) = session.request(turn);
        assert_eq!(system, session.request(0).0, "S3 keeps its bytes");
        assert!(!system.contains("secret text") && !user.contains("secret text"));
        assert!(!user.contains("project_rules_changed"), "{user}");
    }
    assert_eq!(session.rules_digest().await, digest);
}

#[tokio::test]
async fn an_edit_on_an_untrusted_workspace_names_the_notice_in_the_note() {
    let dir = tempfile::tempdir().unwrap();
    write_agents(&dir, &rules("rule 10"));
    let mut session = Session::start_with(&dir, false).await;
    session.turn().await;
    write_agents(&dir, &rules("rule ten edited"));
    session.turn().await;
    let (_, user) = session.request(1);
    assert!(user.contains("project_rules_changed"), "{user}");
    assert!(user.contains("untrusted repository text"), "{user}");
}

#[tokio::test]
async fn an_unreadable_record_is_refrozen_with_a_higher_revision_and_a_warning() {
    let dir = tempfile::tempdir().unwrap();
    write_agents(&dir, &rules("rule 10"));
    let mut session = Session::start(&dir).await;
    session.turn().await;
    session
        .store
        .mutate_chat("s1", 5, |chat| {
            chat.metadata[PROJECT_RULES_METADATA_KEY] = json!({"revision": 7, "snapshot": 3});
            Ok(())
        })
        .await
        .unwrap();
    let turn_id = session.turn().await;
    assert_eq!(
        session.warnings(&turn_id).await,
        vec!["project_rules_record_unreadable"]
    );
    let chat = session.store.get_chat("s1").await.unwrap().unwrap();
    assert_eq!(chat.metadata[PROJECT_RULES_METADATA_KEY]["revision"], 8);
    let again = session.turn().await;
    assert!(session.warnings(&again).await.is_empty());
}
