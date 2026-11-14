//! The MCP configuration context's durable repository port (ADR 0024 §1-§3).
//!
//! [`crate::McpConfigurationService`] drives the settings document that holds
//! the server declarations, the generation counters that fence live runtimes,
//! and the guarded snapshot commits that replace both atomically, all against
//! this trait. It never names a concrete store: the SQLite adapter
//! (`agentkit-store`) implements it, `agentkit-testkit` holds the in-memory
//! twin, and both run the same conformance suite.
//!
//! The port speaks in values the service already handles as JSON: one
//! settings row read, two compare-and-set writes, and the opaque actor
//! identity the process-global port registries key scopes by. Nothing here
//! reaches past what the service calls on a store today.
//!
//! Faults are the adapter's own type, reached only through
//! [`RepositoryFault`] predicates (ADR 0024 §3): the service never matches a
//! concrete error enum, so an adapter is free to organise its failures as it
//! likes as long as the conformance suite's predicate answers hold.

use async_trait::async_trait;
use serde_json::Value;
use std::any::Any;
use std::sync::Weak;

pub use agentkit_engine::RepositoryFault;

/// One guarded settings write of a [`McpConfigRepository::compare_and_set_settings`]
/// commit; `None` deletes the row. Moved from `agentkit-store`, which
/// re-exports it so its own API names the very same type.
#[derive(Clone, Debug)]
pub struct SettingWrite {
    pub namespace: String,
    pub key: String,
    pub value: Option<Value>,
}

/// The outcome of one [`McpConfigRepository::compare_and_set_settings`]
/// commit: applied, refused by the guard, or aborted with the reason the
/// rollback was proved by. The store's own non-generic
/// `agentkit_store::SettingsCommit` stays what its direct callers use; this
/// is the port-side spelling, generic over the adapter's fault so no
/// concrete error enum crosses the boundary (ADR 0024 §3).
#[derive(Debug)]
pub enum SettingsCommit<E> {
    Applied,
    Conflict,
    /// No writes survived, proved by a successful explicit rollback.
    Aborted(E),
}

/// Opaque identity that does not keep a repository (nor its SQLite actor and
/// file lock) alive. The replacement for `agentkit_store::StoreActorIdentity`
/// on this side of the port: the adapter wraps a weak handle to whatever
/// backs the repository, and liveness plus pointer equality give the same
/// "same live actor" answer the store's identity did.
pub struct RepositoryIdentity(Weak<dyn Any + Send + Sync>);

impl Clone for RepositoryIdentity {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl std::fmt::Debug for RepositoryIdentity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RepositoryIdentity")
            .finish_non_exhaustive()
    }
}

impl RepositoryIdentity {
    /// Wraps a weak handle to the repository's backing actor. The adapter
    /// calls this; the service only compares identities.
    pub fn from_handle(handle: Weak<dyn Any + Send + Sync>) -> Self {
        Self(handle)
    }

    /// True while the backing actor is alive.
    pub fn is_alive(&self) -> bool {
        self.0.strong_count() != 0
    }

    /// True only for handles sharing the same live backing actor.
    pub fn matches<R: McpConfigRepository>(&self, repository: &R) -> bool {
        Weak::ptr_eq(&self.0, &repository.actor_identity().0)
    }
}

/// The durable settings store the MCP configuration service drives. See the
/// module docs.
#[async_trait]
pub trait McpConfigRepository: Clone + Send + Sync + 'static {
    /// The adapter's fault type, branchable only through [`RepositoryFault`].
    type Error: RepositoryFault;

    /// One settings row, or `None` when the namespace/key was never written.
    async fn get_setting(&self, namespace: &str, key: &str) -> Result<Option<Value>, Self::Error>;

    /// Replaces one settings value only while its committed value still
    /// matches `expected`; reports whether it wrote.
    async fn compare_and_set_setting(
        &self,
        namespace: &str,
        key: &str,
        expected: Option<Value>,
        next: Value,
    ) -> Result<bool, Self::Error>;

    /// Commits a guarded snapshot and its companion settings as one durable
    /// unit. An outer error cannot prove rollback; only
    /// [`SettingsCommit::Aborted`] does.
    async fn compare_and_set_settings(
        &self,
        namespace: &str,
        key: &str,
        expected: Option<Value>,
        writes: Vec<SettingWrite>,
    ) -> Result<SettingsCommit<Self::Error>, Self::Error>;

    /// The opaque live-actor identity the process-global port registries
    /// key scopes and runtime bindings by.
    fn actor_identity(&self) -> RepositoryIdentity;
}
