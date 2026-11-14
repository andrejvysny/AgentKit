//! The SQLite store as the MCP configuration context's repository (the
//! adapter side of the `agentkit_mcp::McpConfigRepository` port).
//!
//! Every method forwards to the inherent method of the same name; the SQL
//! and the inherent API are unchanged. `SettingWrite` is the very type the
//! port speaks in (this crate re-exports it), and the commit result is only
//! re-wrapped in the port's fault-generic spelling.
use crate::{SqliteStore, StoreError};
use agentkit_mcp::{McpConfigRepository, RepositoryIdentity, SettingWrite, SettingsCommit};
use async_trait::async_trait;
use serde_json::Value;
use std::any::Any;
use std::sync::Arc;

#[async_trait]
impl McpConfigRepository for SqliteStore {
    type Error = StoreError;

    async fn get_setting(&self, namespace: &str, key: &str) -> Result<Option<Value>, StoreError> {
        SqliteStore::get_setting(self, namespace, key).await
    }

    async fn compare_and_set_setting(
        &self,
        namespace: &str,
        key: &str,
        expected: Option<Value>,
        next: Value,
    ) -> Result<bool, StoreError> {
        SqliteStore::compare_and_set_setting(self, namespace, key, expected, next).await
    }

    async fn compare_and_set_settings(
        &self,
        namespace: &str,
        key: &str,
        expected: Option<Value>,
        writes: Vec<SettingWrite>,
    ) -> Result<SettingsCommit<StoreError>, StoreError> {
        SqliteStore::compare_and_set_settings(self, namespace, key, expected, writes)
            .await
            .map(|commit| match commit {
                crate::SettingsCommit::Applied => SettingsCommit::Applied,
                crate::SettingsCommit::Conflict => SettingsCommit::Conflict,
                crate::SettingsCommit::Aborted(error) => SettingsCommit::Aborted(error),
            })
    }

    fn actor_identity(&self) -> RepositoryIdentity {
        let erased: Arc<dyn Any + Send + Sync> = self.worker.clone();
        RepositoryIdentity::from_handle(Arc::downgrade(&erased))
    }
}
