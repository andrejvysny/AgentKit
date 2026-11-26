//! In-memory `WorkspaceRepository` (ADR 0038). One mutex guards the whole
//! state, so every call is one atomic step: a refusal is decided before
//! anything is written and changes nothing.

use std::sync::{Arc, Mutex, MutexGuard};

use agentkit_workspaces::{
    AgentId, DenialRecord, Generation, GrantId, LeaseAcquire, LeaseHolder, LeaseInfo, LeaseToken,
    Member, NetworkGrant, NetworkRequest, NewWorkspace, Operation, OperationId, OperationUpdate,
    RepositoryError, RequestDecided, RequestDecision, RequestId, RequestState, Workspace,
    WorkspaceId, WorkspaceKey, WorkspaceRepository, WorkspaceState, WorkspaceUpsert,
};
use async_trait::async_trait;

mod state;

use state::{Outcome, State, conflict, expiry, invalid, lost};

/// Clones share one state.
#[derive(Clone, Default)]
pub struct InMemoryWorkspaceRepository {
    state: Arc<Mutex<State>>,
}

impl InMemoryWorkspaceRepository {
    /// An empty repository.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Run a fenced workspace write: `change` sees the leased record and
    /// returns its new value, or a refusal that leaves it untouched.
    fn fenced_write(
        &self,
        lease: &LeaseToken,
        now: i64,
        change: impl FnOnce(&State, Workspace) -> Outcome<Workspace>,
    ) -> Outcome<Workspace> {
        let mut state = self.lock();
        let workspace = state.fenced(lease, now)?;
        let workspace = change(&state, workspace)?;
        Ok(state.store(workspace))
    }
}

fn sorted<T: Clone, K: Ord>(items: impl Iterator<Item = T>, key: impl Fn(&T) -> K) -> Vec<T> {
    let mut items: Vec<T> = items.collect();
    items.sort_by_key(|item| key(item));
    items
}

#[async_trait]
impl WorkspaceRepository for InMemoryWorkspaceRepository {
    async fn create_or_get_workspace(&self, new: &NewWorkspace) -> Outcome<WorkspaceUpsert> {
        let mut state = self.lock();
        if let Some(existing) = state.live_by_key(&new.key) {
            return Ok(WorkspaceUpsert {
                workspace: existing.clone(),
                created: false,
            });
        }
        let workspace = state.create(new)?;
        Ok(WorkspaceUpsert {
            workspace,
            created: true,
        })
    }

    async fn workspace(&self, id: &WorkspaceId) -> Outcome<Option<Workspace>> {
        Ok(self.lock().workspaces.get(id).cloned())
    }

    async fn workspace_by_key(&self, key: &WorkspaceKey) -> Outcome<Option<Workspace>> {
        Ok(self.lock().live_by_key(key).cloned())
    }

    async fn list_workspaces(&self) -> Outcome<Vec<Workspace>> {
        let state = self.lock();
        Ok(sorted(state.workspaces.values().cloned(), |ws| {
            (ws.created_at, ws.id.clone())
        }))
    }

    async fn touch_workspace(&self, id: &WorkspaceId, now: i64) -> Outcome<()> {
        let mut state = self.lock();
        let mut workspace = state.workspace(id)?.clone();
        workspace.last_used_at = workspace.last_used_at.max(now);
        state.store(workspace);
        Ok(())
    }

    async fn set_holds_user_files(
        &self,
        lease: &LeaseToken,
        value: bool,
        now: i64,
    ) -> Outcome<Workspace> {
        self.fenced_write(lease, now, |_, mut ws| {
            if ws.state == WorkspaceState::Deleted {
                return Err(conflict(&ws));
            }
            ws.holds_user_files = value;
            Ok(ws)
        })
    }

    async fn mark_ready(&self, lease: &LeaseToken, now: i64) -> Outcome<Workspace> {
        self.fenced_write(lease, now, |state, mut ws| {
            if !matches!(ws.state, WorkspaceState::Busy | WorkspaceState::Ready) {
                return Err(conflict(&ws));
            }
            state.open_operation(&ws)?;
            state.uncertain_operation(&ws)?;
            ws.state = WorkspaceState::Ready;
            Ok(ws)
        })
    }

    async fn fence(
        &self,
        lease: &LeaseToken,
        reason: &str,
        generation: Generation,
        now: i64,
    ) -> Outcome<Workspace> {
        self.fenced_write(lease, now, |_, mut ws| {
            let earlier = match &ws.state {
                WorkspaceState::Deleted => return Err(conflict(&ws)),
                WorkspaceState::Fenced { generation, .. } => *generation,
                WorkspaceState::Ready | WorkspaceState::Busy => ws.generation,
            };
            ws.state = WorkspaceState::Fenced {
                reason: reason.to_owned(),
                generation: generation.max(earlier).max(ws.generation),
            };
            Ok(ws)
        })
    }

    async fn unfence(&self, lease: &LeaseToken, now: i64) -> Outcome<Workspace> {
        self.fenced_write(lease, now, |state, mut ws| {
            if !matches!(ws.state, WorkspaceState::Fenced { .. }) {
                return Err(conflict(&ws));
            }
            state.open_operation(&ws)?;
            ws.state = WorkspaceState::Ready;
            Ok(ws)
        })
    }

    async fn advance_generation(
        &self,
        lease: &LeaseToken,
        expected_current: Generation,
        image_digest: &str,
        now: i64,
    ) -> Outcome<Workspace> {
        self.fenced_write(lease, now, |state, mut ws| {
            if ws.state == WorkspaceState::Deleted {
                return Err(conflict(&ws));
            }
            if ws.generation != expected_current {
                return Err(RepositoryError::StaleGeneration {
                    workspace_id: ws.id.clone(),
                    expected: expected_current,
                    current: ws.generation,
                });
            }
            state.open_operation(&ws)?;
            ws.generation = ws
                .generation
                .next()
                .ok_or_else(|| invalid("generation overflow"))?;
            ws.image_digest = image_digest.to_owned();
            if ws.state == WorkspaceState::Busy {
                ws.state = WorkspaceState::Ready;
            }
            Ok(ws)
        })
    }

    async fn advance_and_unfence(
        &self,
        lease: &LeaseToken,
        expected_current: Generation,
        next: Generation,
        image_digest: &str,
        now: i64,
    ) -> Outcome<Workspace> {
        self.fenced_write(lease, now, |state, mut ws| {
            let floor = match &ws.state {
                WorkspaceState::Deleted => return Err(conflict(&ws)),
                WorkspaceState::Fenced { generation, .. } => (*generation).max(ws.generation),
                WorkspaceState::Ready | WorkspaceState::Busy => ws.generation,
            };
            if ws.generation != expected_current {
                return Err(RepositoryError::StaleGeneration {
                    workspace_id: ws.id.clone(),
                    expected: expected_current,
                    current: ws.generation,
                });
            }
            if next <= floor {
                return Err(invalid(format!(
                    "generation {next} does not move past generation {floor}"
                )));
            }
            state.open_operation(&ws)?;
            ws.generation = next;
            ws.image_digest = image_digest.to_owned();
            ws.state = WorkspaceState::Ready;
            Ok(ws)
        })
    }

    async fn mark_deleted(&self, lease: &LeaseToken, now: i64) -> Outcome<Workspace> {
        self.fenced_write(lease, now, |_, mut ws| {
            if !matches!(
                ws.state,
                WorkspaceState::Ready | WorkspaceState::Fenced { .. }
            ) {
                return Err(conflict(&ws));
            }
            ws.state = WorkspaceState::Deleted;
            Ok(ws)
        })
    }

    async fn acquire_lease(
        &self,
        workspace_id: &WorkspaceId,
        holder: &LeaseHolder,
        token: &str,
        now: i64,
        ttl_ms: i64,
    ) -> Outcome<LeaseAcquire> {
        self.lock()
            .acquire(workspace_id, holder, token, now, ttl_ms)
    }

    async fn renew_lease(&self, lease: &LeaseToken, now: i64, ttl_ms: i64) -> Outcome<LeaseToken> {
        let mut state = self.lock();
        state.check_lease(lease, now)?;
        let expires_at = expiry(now, ttl_ms)?;
        if let Some(stored) = state.leases.get_mut(&lease.workspace_id) {
            stored.expires_at = expires_at;
        }
        Ok(LeaseToken {
            expires_at,
            ..lease.clone()
        })
    }

    async fn release_lease(&self, lease: &LeaseToken) -> Outcome<()> {
        let mut state = self.lock();
        match state.leases.get(&lease.workspace_id) {
            Some(stored) if stored.token == lease.token && stored.fencing == lease.fencing => {
                state.leases.remove(&lease.workspace_id);
                Ok(())
            }
            _ => Err(lost(&lease.workspace_id)),
        }
    }

    async fn lease(&self, workspace_id: &WorkspaceId, now: i64) -> Outcome<Option<LeaseInfo>> {
        let state = self.lock();
        Ok(state
            .leases
            .get(workspace_id)
            .filter(|stored| stored.expires_at > now)
            .map(|stored| stored.info(workspace_id)))
    }

    async fn list_leases(&self) -> Outcome<Vec<LeaseInfo>> {
        let state = self.lock();
        Ok(state
            .leases
            .iter()
            .map(|(id, stored)| stored.info(id))
            .collect())
    }

    async fn break_lease(&self, workspace_id: &WorkspaceId, fencing: u64) -> Outcome<bool> {
        let mut state = self.lock();
        let matches = state
            .leases
            .get(workspace_id)
            .is_some_and(|stored| stored.fencing == fencing);
        if matches {
            state.leases.remove(workspace_id);
        }
        Ok(matches)
    }

    async fn add_member(&self, member: &Member) -> Outcome<Member> {
        let mut state = self.lock();
        state.workspace(&member.workspace_id)?;
        let key = (member.workspace_id.clone(), member.agent_id.clone());
        Ok(state
            .members
            .entry(key)
            .or_insert_with(|| member.clone())
            .clone())
    }

    async fn remove_member(&self, workspace_id: &WorkspaceId, agent_id: &AgentId) -> Outcome<bool> {
        let key = (workspace_id.clone(), agent_id.clone());
        Ok(self.lock().members.remove(&key).is_some())
    }

    async fn member(
        &self,
        workspace_id: &WorkspaceId,
        agent_id: &AgentId,
    ) -> Outcome<Option<Member>> {
        let key = (workspace_id.clone(), agent_id.clone());
        Ok(self.lock().members.get(&key).cloned())
    }

    async fn list_members(&self, workspace_id: &WorkspaceId) -> Outcome<Vec<Member>> {
        let state = self.lock();
        let members = state
            .members
            .values()
            .filter(|member| &member.workspace_id == workspace_id)
            .cloned();
        Ok(sorted(members, |member| {
            (member.added_at, member.agent_id.clone())
        }))
    }

    async fn insert_operation(
        &self,
        lease: &LeaseToken,
        op: &Operation,
        now: i64,
    ) -> Outcome<bool> {
        self.lock().insert_operation(lease, op, now)
    }

    async fn update_operation(
        &self,
        lease: &LeaseToken,
        id: &OperationId,
        update: &OperationUpdate,
        now: i64,
    ) -> Outcome<Operation> {
        self.lock().update_operation(lease, id, update, now)
    }

    async fn operation(&self, id: &OperationId) -> Outcome<Option<Operation>> {
        Ok(self.lock().operations.get(id).cloned())
    }

    async fn list_operations(
        &self,
        workspace_id: &WorkspaceId,
        limit: usize,
    ) -> Outcome<Vec<Operation>> {
        let state = self.lock();
        let ops = state
            .operations
            .values()
            .filter(|op| &op.workspace_id == workspace_id)
            .cloned();
        let mut ops = sorted(ops, |op| (op.started_at, op.id.clone()));
        ops.reverse();
        ops.truncate(limit);
        Ok(ops)
    }

    async fn list_open_operations(&self) -> Outcome<Vec<Operation>> {
        let state = self.lock();
        let ops = state
            .operations
            .values()
            .filter(|op| op.phase.is_open())
            .cloned();
        Ok(sorted(ops, |op| (op.started_at, op.id.clone())))
    }

    async fn insert_grant(&self, grant: &NetworkGrant) -> Outcome<NetworkGrant> {
        self.lock().insert_grant(grant)
    }

    async fn revoke_grant(&self, id: &GrantId, now: i64) -> Outcome<NetworkGrant> {
        let mut state = self.lock();
        let grant = state
            .grants
            .get_mut(id)
            .ok_or_else(|| RepositoryError::GrantNotFound(id.clone()))?;
        grant.revoked_at = grant.revoked_at.or(Some(now));
        Ok(grant.clone())
    }

    async fn list_active_grants(&self, workspace_id: &WorkspaceId) -> Outcome<Vec<NetworkGrant>> {
        let state = self.lock();
        let grants = state
            .grants
            .values()
            .filter(|grant| &grant.workspace_id == workspace_id && grant.is_active())
            .cloned();
        Ok(sorted(grants, |grant| (grant.granted_at, grant.id.clone())))
    }

    async fn record_denial(&self, denial: &DenialRecord) -> Outcome<NetworkRequest> {
        self.lock().record_denial(denial)
    }

    async fn decide_request(
        &self,
        id: &RequestId,
        decision: &RequestDecision,
    ) -> Outcome<RequestDecided> {
        self.lock().decide(id, decision)
    }

    async fn list_pending_requests(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Outcome<Vec<NetworkRequest>> {
        let state = self.lock();
        let requests = state
            .requests
            .values()
            .filter(|request| {
                &request.workspace_id == workspace_id && request.state == RequestState::Pending
            })
            .cloned();
        Ok(sorted(requests, |request| {
            (request.first_seen, request.id.clone())
        }))
    }
}
