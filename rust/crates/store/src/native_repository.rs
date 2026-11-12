//! The SQLite store as the native lane's run repository (the adapter side
//! of the `agentkit_native::NativeRunRepository` port, phase N1).
//!
//! Every method forwards to the inherent method of the same name; the SQL
//! and the inherent API are unchanged.

use crate::{SqliteStore, StoreError};
use agentkit_native::{
    BindNativeSession, CommitContinuation, NativeContinuationBinding, NativeRunRepository,
    NativeSessionBinding,
};
use async_trait::async_trait;

#[async_trait]
impl NativeRunRepository for SqliteStore {
    type Error = StoreError;

    async fn bind_native_session(
        &self,
        input: BindNativeSession,
    ) -> Result<NativeSessionBinding, Self::Error> {
        SqliteStore::bind_native_session(self, input).await
    }

    async fn observe_native_session(
        &self,
        attempt_id: &str,
        vendor_session_id: &str,
        now: i64,
    ) -> Result<NativeSessionBinding, Self::Error> {
        SqliteStore::observe_native_session(self, attempt_id, vendor_session_id, now).await
    }

    async fn current_native_binding(
        &self,
        task_id: &str,
    ) -> Result<Option<NativeSessionBinding>, Self::Error> {
        SqliteStore::current_native_binding(self, task_id).await
    }

    async fn invalidate_interactions(
        &self,
        attempt_id: &str,
        now: i64,
    ) -> Result<usize, Self::Error> {
        SqliteStore::invalidate_interactions(self, attempt_id, now).await
    }

    async fn active_continuation(
        &self,
        conversation: &str,
        parent: &str,
        history_position: i64,
        backend: &str,
        executable: &str,
        config_identity: &str,
        system_prompt_revision: Option<&str>,
    ) -> Result<Option<NativeContinuationBinding>, Self::Error> {
        SqliteStore::active_continuation(
            self,
            conversation,
            parent,
            history_position,
            backend,
            executable,
            config_identity,
            system_prompt_revision,
        )
        .await
    }

    async fn claim_selected_continuation(
        &self,
        selected: &NativeContinuationBinding,
        claimant_task_id: &str,
        claimant_attempt_id: &str,
        now: i64,
    ) -> Result<Option<NativeContinuationBinding>, Self::Error> {
        SqliteStore::claim_selected_continuation(
            self,
            selected,
            claimant_task_id,
            claimant_attempt_id,
            now,
        )
        .await
    }

    async fn commit_continuation(
        &self,
        input: CommitContinuation,
    ) -> Result<NativeContinuationBinding, Self::Error> {
        SqliteStore::commit_continuation(self, input).await
    }

    async fn resumable_continuation(
        &self,
        conversation: &str,
        parent: &str,
        history_position: i64,
        backend: &str,
        executable: &str,
        config_identity: &str,
        system_prompt_revision: Option<&str>,
        attempt_id: &str,
    ) -> Result<Option<NativeContinuationBinding>, Self::Error> {
        SqliteStore::resumable_continuation(
            self,
            conversation,
            parent,
            history_position,
            backend,
            executable,
            config_identity,
            system_prompt_revision,
            attempt_id,
        )
        .await
    }

    async fn release_continuation_claim(
        &self,
        binding_id: &str,
        claimant_attempt_id: &str,
    ) -> Result<Option<NativeContinuationBinding>, Self::Error> {
        SqliteStore::release_continuation_claim(self, binding_id, claimant_attempt_id).await
    }

    async fn mark_continuation_uncertain(
        &self,
        binding_id: &str,
        claimant_attempt_id: &str,
    ) -> Result<Option<NativeContinuationBinding>, Self::Error> {
        SqliteStore::mark_continuation_uncertain(self, binding_id, claimant_attempt_id).await
    }
}
