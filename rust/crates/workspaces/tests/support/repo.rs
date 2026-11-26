//! A repository wrapper that lets virtual time jump right after a
//! successful `insert_operation`: the worker stalls between writing the
//! row and dispatching it, long enough for its lease to expire.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering::SeqCst};

use agentkit_testkit::{InMemoryWorkspaceRepository, ManualClock};
use agentkit_workspaces::{
    AgentId, DenialRecord, Generation, GrantId, LeaseAcquire, LeaseHolder, LeaseInfo, LeaseToken,
    Member, NetworkGrant, NetworkRequest, NewWorkspace, Operation, OperationId, OperationUpdate,
    RepositoryError, RequestDecided, RequestDecision, RequestId, Workspace, WorkspaceId,
    WorkspaceKey, WorkspaceRepository, WorkspaceUpsert,
};
use async_trait::async_trait;

type Outcome<T> = Result<T, RepositoryError>;

/// The in-memory twin, plus one switch.
pub struct StallingRepo {
    pub inner: Arc<InMemoryWorkspaceRepository>,
    clock: Arc<ManualClock>,
    /// After the next insert that creates a row, the clock jumps this far.
    pub stall_after_insert_ms: AtomicI64,
}

impl StallingRepo {
    pub fn new(inner: Arc<InMemoryWorkspaceRepository>, clock: Arc<ManualClock>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            clock,
            stall_after_insert_ms: AtomicI64::new(0),
        })
    }
}

#[async_trait]
impl WorkspaceRepository for StallingRepo {
    async fn create_or_get_workspace(&self, new: &NewWorkspace) -> Outcome<WorkspaceUpsert> {
        self.inner.create_or_get_workspace(new).await
    }
    async fn workspace(&self, id: &WorkspaceId) -> Outcome<Option<Workspace>> {
        self.inner.workspace(id).await
    }
    async fn workspace_by_key(&self, key: &WorkspaceKey) -> Outcome<Option<Workspace>> {
        self.inner.workspace_by_key(key).await
    }
    async fn list_workspaces(&self) -> Outcome<Vec<Workspace>> {
        self.inner.list_workspaces().await
    }
    async fn touch_workspace(&self, id: &WorkspaceId, now: i64) -> Outcome<()> {
        self.inner.touch_workspace(id, now).await
    }
    async fn set_holds_user_files(
        &self,
        lease: &LeaseToken,
        value: bool,
        now: i64,
    ) -> Outcome<Workspace> {
        self.inner.set_holds_user_files(lease, value, now).await
    }
    async fn mark_ready(&self, lease: &LeaseToken, now: i64) -> Outcome<Workspace> {
        self.inner.mark_ready(lease, now).await
    }
    async fn fence(
        &self,
        lease: &LeaseToken,
        reason: &str,
        generation: Generation,
        now: i64,
    ) -> Outcome<Workspace> {
        self.inner.fence(lease, reason, generation, now).await
    }
    async fn unfence(&self, lease: &LeaseToken, now: i64) -> Outcome<Workspace> {
        self.inner.unfence(lease, now).await
    }
    async fn advance_generation(
        &self,
        lease: &LeaseToken,
        expected_current: Generation,
        image_digest: &str,
        now: i64,
    ) -> Outcome<Workspace> {
        self.inner
            .advance_generation(lease, expected_current, image_digest, now)
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
        self.inner
            .advance_and_unfence(lease, expected_current, next, image_digest, now)
            .await
    }
    async fn mark_deleted(&self, lease: &LeaseToken, now: i64) -> Outcome<Workspace> {
        self.inner.mark_deleted(lease, now).await
    }
    async fn acquire_lease(
        &self,
        workspace_id: &WorkspaceId,
        holder: &LeaseHolder,
        token: &str,
        now: i64,
        ttl_ms: i64,
    ) -> Outcome<LeaseAcquire> {
        self.inner
            .acquire_lease(workspace_id, holder, token, now, ttl_ms)
            .await
    }
    async fn renew_lease(&self, lease: &LeaseToken, now: i64, ttl_ms: i64) -> Outcome<LeaseToken> {
        self.inner.renew_lease(lease, now, ttl_ms).await
    }
    async fn release_lease(&self, lease: &LeaseToken) -> Outcome<()> {
        self.inner.release_lease(lease).await
    }
    async fn lease(&self, workspace_id: &WorkspaceId, now: i64) -> Outcome<Option<LeaseInfo>> {
        self.inner.lease(workspace_id, now).await
    }
    async fn list_leases(&self) -> Outcome<Vec<LeaseInfo>> {
        self.inner.list_leases().await
    }
    async fn break_lease(&self, workspace_id: &WorkspaceId, fencing: u64) -> Outcome<bool> {
        self.inner.break_lease(workspace_id, fencing).await
    }
    async fn add_member(&self, member: &Member) -> Outcome<Member> {
        self.inner.add_member(member).await
    }
    async fn remove_member(&self, workspace_id: &WorkspaceId, agent_id: &AgentId) -> Outcome<bool> {
        self.inner.remove_member(workspace_id, agent_id).await
    }
    async fn member(
        &self,
        workspace_id: &WorkspaceId,
        agent_id: &AgentId,
    ) -> Outcome<Option<Member>> {
        self.inner.member(workspace_id, agent_id).await
    }
    async fn list_members(&self, workspace_id: &WorkspaceId) -> Outcome<Vec<Member>> {
        self.inner.list_members(workspace_id).await
    }
    async fn insert_operation(
        &self,
        lease: &LeaseToken,
        op: &Operation,
        now: i64,
    ) -> Outcome<bool> {
        let created = self.inner.insert_operation(lease, op, now).await?;
        if created {
            self.clock
                .advance(self.stall_after_insert_ms.swap(0, SeqCst));
        }
        Ok(created)
    }
    async fn update_operation(
        &self,
        lease: &LeaseToken,
        id: &OperationId,
        update: &OperationUpdate,
        now: i64,
    ) -> Outcome<Operation> {
        self.inner.update_operation(lease, id, update, now).await
    }
    async fn operation(&self, id: &OperationId) -> Outcome<Option<Operation>> {
        self.inner.operation(id).await
    }
    async fn list_operations(
        &self,
        workspace_id: &WorkspaceId,
        limit: usize,
    ) -> Outcome<Vec<Operation>> {
        self.inner.list_operations(workspace_id, limit).await
    }
    async fn list_open_operations(&self) -> Outcome<Vec<Operation>> {
        self.inner.list_open_operations().await
    }
    async fn insert_grant(&self, grant: &NetworkGrant) -> Outcome<NetworkGrant> {
        self.inner.insert_grant(grant).await
    }
    async fn revoke_grant(&self, id: &GrantId, now: i64) -> Outcome<NetworkGrant> {
        self.inner.revoke_grant(id, now).await
    }
    async fn list_active_grants(&self, workspace_id: &WorkspaceId) -> Outcome<Vec<NetworkGrant>> {
        self.inner.list_active_grants(workspace_id).await
    }
    async fn record_denial(&self, denial: &DenialRecord) -> Outcome<NetworkRequest> {
        self.inner.record_denial(denial).await
    }
    async fn decide_request(
        &self,
        id: &RequestId,
        decision: &RequestDecision,
    ) -> Outcome<RequestDecided> {
        self.inner.decide_request(id, decision).await
    }
    async fn list_pending_requests(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Outcome<Vec<NetworkRequest>> {
        self.inner.list_pending_requests(workspace_id).await
    }
}
