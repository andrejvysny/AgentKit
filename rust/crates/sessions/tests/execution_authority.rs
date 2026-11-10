//! The guard must not confuse "this chat is not a session" with "the
//! session's state is unreadable".
//!
//! A REST-minted chat is a plain chat: no session descriptor, no durable
//! execution state. The authority answers ABSENCE (the guard falls back to
//! its configured mode) — an error there used to fail every JS tool on a
//! REST-minted chat closed with `session_not_found`. Only a real read
//! failure — a session whose persisted execution state is malformed — stays
//! a hard error, because falling back there would silently replace a human
//! decision with process defaults.

use agentkit_engine::SystemRuntime;
use agentkit_sessions::{
    AgentSessionService, AgentSessionServiceDeps, CodingPermissionAuthority, CreateSessionInput,
    PermissionContext, SessionExecutionAuthority,
};
use agentkit_store::{EXECUTION_METADATA_KEY, SqliteStore};
use serde_json::json;
use std::sync::Arc;

fn fixture() -> (SqliteStore, AgentSessionService) {
    let store = SqliteStore::memory(8).unwrap();
    let service = AgentSessionService::new(AgentSessionServiceDeps {
        store: store.clone(),
        runtime: Arc::new(SystemRuntime),
        plan_approval: None,
        wake: None,
    });
    (store, service)
}

fn context(chat: &str) -> PermissionContext {
    PermissionContext {
        chat_id: Some(chat.to_owned()),
        run_id: Some("run-1".to_owned()),
    }
}

#[tokio::test]
async fn non_session_chats_answer_absence_never_error() {
    let (store, service) = fixture();
    let authority = SessionExecutionAuthority::new(service.clone());
    store
        .create_chat("rest-chat", None, json!({}), 1000)
        .await
        .unwrap();

    assert_eq!(
        authority.mode_of(&context("rest-chat")).await.unwrap(),
        None
    );
    assert_eq!(
        authority
            .plan_approved(Some("rest-chat"), "digest")
            .await
            .unwrap(),
        None
    );
    // A chat id that does not exist at all is the same absence.
    assert_eq!(authority.mode_of(&context("nope")).await.unwrap(), None);
    assert_eq!(
        authority
            .plan_approved(Some("nope"), "digest")
            .await
            .unwrap(),
        None
    );
    // A session inherits its profile permission mode.
    service
        .create_session(&CreateSessionInput {
            session_id: Some("session-1".into()),
            profile_id: "coding".into(),
            ..CreateSessionInput::default()
        })
        .await
        .unwrap();
    assert_eq!(
        authority
            .mode_of(&context("session-1"))
            .await
            .unwrap()
            .map(|mode| mode.as_str().to_owned()),
        Some("ask".to_owned())
    );
}

#[tokio::test]
async fn a_session_durable_mode_answers_and_malformed_state_stays_an_error() {
    let (store, service) = fixture();
    let authority = SessionExecutionAuthority::new(service.clone());
    service
        .create_session(&CreateSessionInput {
            session_id: Some("session-1".into()),
            profile_id: "coding".into(),
            ..CreateSessionInput::default()
        })
        .await
        .unwrap();
    service
        .set_execution_mode("session-1", "auto_edit")
        .await
        .unwrap();
    assert_eq!(
        authority
            .mode_of(&context("session-1"))
            .await
            .unwrap()
            .map(|mode| mode.as_str().to_owned()),
        Some("auto_edit".into())
    );

    // Corrupt the durable state behind the service's back: the authority
    // must ERROR, not answer "no opinion" — the guard then refuses instead
    // of silently using the configured mode.
    store
        .mutate_chat("session-1", 2000, move |chat| {
            chat.metadata
                .as_object_mut()
                .unwrap()
                .insert(EXECUTION_METADATA_KEY.to_owned(), json!({"version": 1}));
            Ok(())
        })
        .await
        .unwrap();
    assert!(authority.mode_of(&context("session-1")).await.is_err());
    assert!(
        authority
            .plan_approved(Some("session-1"), "digest")
            .await
            .is_err()
    );
}
