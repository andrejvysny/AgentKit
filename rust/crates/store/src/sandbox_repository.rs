//! The SQLite store as the workspaces context's `WorkspaceRepository` (ADR
//! 0038, schema version 23).
//!
//! Every call is one unit of work on the store's worker inside one SQLite
//! transaction (a savepoint inside an ambient one): a fenced write checks
//! the lease in the same transaction as the change, and a refusal returns
//! through the dropped transaction, so nothing it touched persists. Every
//! value is a bound parameter. The repository never reads a clock or mints
//! an id: times, ids and lease tokens come from the caller.

mod codec;
mod leases;
mod network;
mod operations;
mod workspaces;

use crate::{SqliteStore, transaction::atomic};
use agentkit_workspaces::{
    AgentId, DenialRecord, Generation, GrantId, LeaseAcquire, LeaseHolder, LeaseInfo, LeaseToken,
    Member, NetworkGrant, NetworkRequest, NewWorkspace, Operation, OperationId, OperationUpdate,
    RepositoryError, RequestDecided, RequestDecision, RequestId, Workspace, WorkspaceId,
    WorkspaceKey, WorkspaceRepository, WorkspaceUpsert,
};
use async_trait::async_trait;
use rusqlite::Connection;

type Outcome<T> = Result<T, RepositoryError>;

/// What a unit of SQL work can fail with before it is mapped onto the
/// port's own error.
#[derive(Debug)]
enum Fault {
    Refused(RepositoryError),
    Sql(rusqlite::Error),
}

impl From<RepositoryError> for Fault {
    fn from(error: RepositoryError) -> Self {
        Self::Refused(error)
    }
}

impl From<rusqlite::Error> for Fault {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sql(error)
    }
}

impl From<Fault> for RepositoryError {
    fn from(fault: Fault) -> Self {
        match fault {
            Fault::Refused(error) => error,
            Fault::Sql(error) => backend(error),
        }
    }
}

type Work<T> = Result<T, Fault>;

fn backend(error: impl std::fmt::Display) -> RepositoryError {
    RepositoryError::Backend {
        reason: error.to_string(),
    }
}

/// Run `work` on the store's worker in one transaction: committed when it
/// succeeds, rolled back when it fails.
async fn run<T, F>(store: &SqliteStore, work: F) -> Outcome<T>
where
    T: Send + 'static,
    F: FnOnce(&Connection) -> Work<T> + Send + 'static,
{
    let outcome = store
        .call(move |db| {
            let transaction = atomic(db)?;
            match work(&transaction) {
                Ok(value) => {
                    transaction.commit()?;
                    Ok(Ok(value))
                }
                Err(fault) => Ok(Err(fault)),
            }
        })
        .await
        .map_err(backend)?;
    outcome.map_err(RepositoryError::from)
}

/// `WorkspaceRepository` over a [`SqliteStore`].
#[derive(Clone)]
pub struct SqliteWorkspaceRepository {
    store: SqliteStore,
}

impl SqliteWorkspaceRepository {
    pub fn new(store: SqliteStore) -> Self {
        Self { store }
    }

    async fn run<T, F>(&self, work: F) -> Outcome<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Work<T> + Send + 'static,
    {
        run(&self.store, work).await
    }
}

#[async_trait]
impl WorkspaceRepository for SqliteWorkspaceRepository {
    async fn create_or_get_workspace(&self, new: &NewWorkspace) -> Outcome<WorkspaceUpsert> {
        let new = new.clone();
        self.run(move |db| workspaces::create_or_get(db, &new))
            .await
    }

    async fn workspace(&self, id: &WorkspaceId) -> Outcome<Option<Workspace>> {
        let id = id.clone();
        self.run(move |db| workspaces::get(db, &id)).await
    }

    async fn workspace_by_key(&self, key: &WorkspaceKey) -> Outcome<Option<Workspace>> {
        let key = key.clone();
        self.run(move |db| workspaces::by_key(db, &key)).await
    }

    async fn list_workspaces(&self) -> Outcome<Vec<Workspace>> {
        self.run(workspaces::list).await
    }

    async fn touch_workspace(&self, id: &WorkspaceId, now: i64) -> Outcome<()> {
        let id = id.clone();
        self.run(move |db| workspaces::touch(db, &id, now)).await
    }

    async fn set_holds_user_files(
        &self,
        lease: &LeaseToken,
        value: bool,
        now: i64,
    ) -> Outcome<Workspace> {
        let lease = lease.clone();
        self.run(move |db| workspaces::set_holds_user_files(db, &lease, value, now))
            .await
    }

    async fn mark_ready(&self, lease: &LeaseToken, now: i64) -> Outcome<Workspace> {
        let lease = lease.clone();
        self.run(move |db| workspaces::mark_ready(db, &lease, now))
            .await
    }

    async fn fence(
        &self,
        lease: &LeaseToken,
        reason: &str,
        generation: Generation,
        now: i64,
    ) -> Outcome<Workspace> {
        let (lease, reason) = (lease.clone(), reason.to_owned());
        self.run(move |db| workspaces::fence(db, &lease, reason, generation, now))
            .await
    }

    async fn advance_and_unfence(
        &self,
        lease: &LeaseToken,
        expected_current: Generation,
        next: Generation,
        image_digest: &str,
        now: i64,
    ) -> Outcome<Workspace> {
        let (lease, digest) = (lease.clone(), image_digest.to_owned());
        self.run(move |db| {
            workspaces::advance_and_unfence(db, &lease, expected_current, next, digest, now)
        })
        .await
    }

    async fn unfence(&self, lease: &LeaseToken, now: i64) -> Outcome<Workspace> {
        let lease = lease.clone();
        self.run(move |db| workspaces::unfence(db, &lease, now))
            .await
    }

    async fn advance_generation(
        &self,
        lease: &LeaseToken,
        expected_current: Generation,
        image_digest: &str,
        now: i64,
    ) -> Outcome<Workspace> {
        let (lease, digest) = (lease.clone(), image_digest.to_owned());
        self.run(move |db| {
            workspaces::advance_generation(db, &lease, expected_current, digest, now)
        })
        .await
    }

    async fn mark_deleted(&self, lease: &LeaseToken, now: i64) -> Outcome<Workspace> {
        let lease = lease.clone();
        self.run(move |db| workspaces::mark_deleted(db, &lease, now))
            .await
    }

    async fn acquire_lease(
        &self,
        workspace_id: &WorkspaceId,
        holder: &LeaseHolder,
        token: &str,
        now: i64,
        ttl_ms: i64,
    ) -> Outcome<LeaseAcquire> {
        let (id, holder, token) = (workspace_id.clone(), holder.clone(), token.to_owned());
        self.run(move |db| leases::acquire(db, &id, &holder, &token, now, ttl_ms))
            .await
    }

    async fn renew_lease(&self, lease: &LeaseToken, now: i64, ttl_ms: i64) -> Outcome<LeaseToken> {
        let lease = lease.clone();
        self.run(move |db| leases::renew(db, &lease, now, ttl_ms))
            .await
    }

    async fn release_lease(&self, lease: &LeaseToken) -> Outcome<()> {
        let lease = lease.clone();
        self.run(move |db| leases::release(db, &lease)).await
    }

    async fn lease(&self, workspace_id: &WorkspaceId, now: i64) -> Outcome<Option<LeaseInfo>> {
        let id = workspace_id.clone();
        self.run(move |db| leases::current(db, &id, now)).await
    }

    async fn list_leases(&self) -> Outcome<Vec<LeaseInfo>> {
        self.run(leases::list).await
    }

    async fn break_lease(&self, workspace_id: &WorkspaceId, fencing: u64) -> Outcome<bool> {
        let id = workspace_id.clone();
        self.run(move |db| leases::break_lease(db, &id, fencing))
            .await
    }

    async fn add_member(&self, member: &Member) -> Outcome<Member> {
        let member = member.clone();
        self.run(move |db| workspaces::add_member(db, &member))
            .await
    }

    async fn remove_member(&self, workspace_id: &WorkspaceId, agent_id: &AgentId) -> Outcome<bool> {
        let (id, agent) = (workspace_id.clone(), agent_id.clone());
        self.run(move |db| workspaces::remove_member(db, &id, &agent))
            .await
    }

    async fn member(
        &self,
        workspace_id: &WorkspaceId,
        agent_id: &AgentId,
    ) -> Outcome<Option<Member>> {
        let (id, agent) = (workspace_id.clone(), agent_id.clone());
        self.run(move |db| workspaces::member(db, &id, &agent))
            .await
    }

    async fn list_members(&self, workspace_id: &WorkspaceId) -> Outcome<Vec<Member>> {
        let id = workspace_id.clone();
        self.run(move |db| workspaces::list_members(db, &id)).await
    }

    async fn insert_operation(
        &self,
        lease: &LeaseToken,
        op: &Operation,
        now: i64,
    ) -> Outcome<bool> {
        let (lease, op) = (lease.clone(), op.clone());
        self.run(move |db| operations::insert(db, &lease, &op, now))
            .await
    }

    async fn update_operation(
        &self,
        lease: &LeaseToken,
        id: &OperationId,
        update: &OperationUpdate,
        now: i64,
    ) -> Outcome<Operation> {
        let (lease, id, update) = (lease.clone(), id.clone(), update.clone());
        self.run(move |db| operations::update(db, &lease, &id, &update, now))
            .await
    }

    async fn operation(&self, id: &OperationId) -> Outcome<Option<Operation>> {
        let id = id.clone();
        self.run(move |db| operations::get(db, &id)).await
    }

    async fn list_operations(
        &self,
        workspace_id: &WorkspaceId,
        limit: usize,
    ) -> Outcome<Vec<Operation>> {
        let id = workspace_id.clone();
        self.run(move |db| operations::list(db, &id, limit)).await
    }

    async fn list_open_operations(&self) -> Outcome<Vec<Operation>> {
        self.run(operations::list_open).await
    }

    async fn insert_grant(&self, grant: &NetworkGrant) -> Outcome<NetworkGrant> {
        let grant = grant.clone();
        self.run(move |db| network::insert_grant(db, &grant)).await
    }

    async fn revoke_grant(&self, id: &GrantId, now: i64) -> Outcome<NetworkGrant> {
        let id = id.clone();
        self.run(move |db| network::revoke_grant(db, &id, now))
            .await
    }

    async fn list_active_grants(&self, workspace_id: &WorkspaceId) -> Outcome<Vec<NetworkGrant>> {
        let id = workspace_id.clone();
        self.run(move |db| network::active_grants(db, &id)).await
    }

    async fn record_denial(&self, denial: &DenialRecord) -> Outcome<NetworkRequest> {
        let denial = denial.clone();
        self.run(move |db| network::record_denial(db, &denial))
            .await
    }

    async fn decide_request(
        &self,
        id: &RequestId,
        decision: &RequestDecision,
    ) -> Outcome<RequestDecided> {
        let (id, decision) = (id.clone(), decision.clone());
        self.run(move |db| network::decide(db, &id, &decision))
            .await
    }

    async fn list_pending_requests(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Outcome<Vec<NetworkRequest>> {
        let id = workspace_id.clone();
        self.run(move |db| network::pending_requests(db, &id)).await
    }
}
