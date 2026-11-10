use agentkit_store::SqliteStore;
use agentkit_testkit::conformance::task_repository;

#[tokio::test]
async fn sqlite_conforms() {
    task_repository::run(|| async { SqliteStore::memory(16).unwrap() }).await;
}
