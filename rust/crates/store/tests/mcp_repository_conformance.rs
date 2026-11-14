//! The SQLite adapter is the behavioural reference for the MCP
//! configuration port (ADR 0024 §6): the suite `agentkit-testkit` runs
//! against its in-memory twin runs here too. The settings rows the port
//! only reads are seeded through the store's own writer, the same split
//! `sessions_repository_conformance` makes.

use agentkit_store::{SqliteStore, StoreError};
use agentkit_testkit::conformance::mcp_repository::{self, McpRepositoryFixture};
use async_trait::async_trait;
use serde_json::Value;

/// The suite's handle over the reference implementation.
#[derive(Clone)]
struct SqliteMcpFixture {
    store: SqliteStore,
}

#[async_trait]
impl McpRepositoryFixture for SqliteMcpFixture {
    type Repo = SqliteStore;
    type Error = StoreError;

    fn repo(&self) -> &SqliteStore {
        &self.store
    }

    fn new_repo(&self) -> SqliteStore {
        // A fresh in-memory database is a fresh actor: never the same live
        // worker the fixture drives.
        SqliteStore::memory(16).expect("fixture store")
    }

    async fn put_setting(
        &self,
        namespace: &str,
        key: &str,
        value: Value,
    ) -> Result<(), StoreError> {
        self.store.set_setting(namespace, key, value).await
    }
}

#[tokio::test]
async fn sqlite_conforms() {
    mcp_repository::run(|| async {
        SqliteMcpFixture {
            store: SqliteStore::memory(16).unwrap(),
        }
    })
    .await;
}
