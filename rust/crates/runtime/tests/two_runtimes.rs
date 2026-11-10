//! Two runtimes in one process share nothing: each owns its store, host,
//! and worker, and shutting one down leaves the other serving.

use std::sync::Arc;

use agentkit_core::{ChatProvider, ChatRequest, ProviderEvent, ProviderStream};
use agentkit_runtime::AgentRuntime;
use agentkit_sessions::CreateSessionInput;
use agentkit_store::SqliteStore;
use tokio_util::sync::CancellationToken;

struct Fake(&'static str);

impl ChatProvider for Fake {
    fn id(&self) -> &str {
        self.0
    }

    fn stream_chat(&self, _: ChatRequest, _: CancellationToken) -> ProviderStream {
        Box::pin(futures::stream::iter([Ok(ProviderEvent::Completed {
            content: "ok".into(),
            tool_calls: vec![],
            finish_reason: "stop".into(),
        })]))
    }
}

async fn build(dir: &tempfile::TempDir, provider: &'static str) -> AgentRuntime {
    let store = SqliteStore::open(dir.path().join("store.sqlite"), 8).expect("open store");
    AgentRuntime::builder(store)
        .default_provider(Arc::new(Fake(provider)))
        .build()
        .await
        .expect("build runtime")
}

fn session(id: &str) -> CreateSessionInput {
    CreateSessionInput {
        session_id: Some(id.into()),
        profile_id: "coding".into(),
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn two_runtimes_coexist_and_shut_down_independently() {
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut a = build(&dir_a, "fake-a").await;
    let mut b = build(&dir_b, "fake-b").await;
    let worker_a = a.take_worker().expect("worker a started");
    let worker_b = b.take_worker().expect("worker b started");

    a.service()
        .create_session(&session("session-a"))
        .await
        .expect("session on a");
    b.service()
        .create_session(&session("session-b"))
        .await
        .expect("session on b");

    worker_a.shutdown().await.expect("shutdown a");

    // The surviving runtime still accepts a session operation; the stores
    // are separate, so a's session is invisible to b.
    b.service()
        .create_session(&session("session-b2"))
        .await
        .expect("b serves after a shutdown");
    assert!(b.service().get_state("session-a").await.is_err());
    assert!(b.service().get_state("session-b").await.is_ok());

    worker_b.shutdown().await.expect("shutdown b");
}
