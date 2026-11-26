//! The twin's state and the port's rules over it. Every method decides a
//! refusal before it writes, so a refusal changes nothing.

use std::collections::BTreeMap;

use agentkit_workspaces::{
    AgentId, DenialRecord, Generation, GrantId, LeaseAcquire, LeaseHolder, LeaseInfo, LeaseToken,
    Member, NetworkGrant, NetworkRequest, NewWorkspace, Operation, OperationId, OperationPhase,
    OperationUpdate, RepositoryError, RequestDecided, RequestDecision, RequestId, RequestState,
    Workspace, WorkspaceId, WorkspaceKey, WorkspaceState,
};

pub(super) type Outcome<T> = Result<T, RepositoryError>;

#[derive(Clone)]
pub(super) struct StoredLease {
    pub(super) holder: LeaseHolder,
    pub(super) token: String,
    pub(super) fencing: u64,
    pub(super) expires_at: i64,
}

impl StoredLease {
    pub(super) fn info(&self, workspace_id: &WorkspaceId) -> LeaseInfo {
        LeaseInfo {
            workspace_id: workspace_id.clone(),
            holder: self.holder.clone(),
            fencing: self.fencing,
            expires_at: self.expires_at,
        }
    }
}

#[derive(Default)]
pub(super) struct State {
    pub(super) workspaces: BTreeMap<WorkspaceId, Workspace>,
    /// Last fencing number issued per workspace; survives release and break.
    pub(super) fencing: BTreeMap<WorkspaceId, u64>,
    pub(super) leases: BTreeMap<WorkspaceId, StoredLease>,
    pub(super) members: BTreeMap<(WorkspaceId, AgentId), Member>,
    pub(super) operations: BTreeMap<OperationId, Operation>,
    pub(super) grants: BTreeMap<GrantId, NetworkGrant>,
    pub(super) requests: BTreeMap<RequestId, NetworkRequest>,
}

pub(super) fn invalid(reason: impl Into<String>) -> RepositoryError {
    RepositoryError::InvalidInput {
        reason: reason.into(),
    }
}

pub(super) fn lost(workspace_id: &WorkspaceId) -> RepositoryError {
    RepositoryError::LeaseLost {
        workspace_id: workspace_id.clone(),
    }
}

pub(super) fn conflict(workspace: &Workspace) -> RepositoryError {
    RepositoryError::WorkspaceStateConflict {
        workspace_id: workspace.id.clone(),
        state: workspace.state.clone(),
    }
}

pub(super) fn expiry(now: i64, ttl_ms: i64) -> Outcome<i64> {
    if ttl_ms < 0 {
        return Err(invalid("lease TTL must not be negative"));
    }
    now.checked_add(ttl_ms)
        .ok_or_else(|| invalid("lease expiry overflow"))
}

impl State {
    pub(super) fn workspace(&self, id: &WorkspaceId) -> Outcome<&Workspace> {
        self.workspaces
            .get(id)
            .ok_or_else(|| RepositoryError::WorkspaceNotFound(id.clone()))
    }

    pub(super) fn live_by_key(&self, key: &WorkspaceKey) -> Option<&Workspace> {
        self.workspaces
            .values()
            .find(|ws| &ws.key == key && ws.state != WorkspaceState::Deleted)
    }

    /// The token is current: same token and fencing, unexpired at `now`.
    pub(super) fn check_lease(&self, lease: &LeaseToken, now: i64) -> Outcome<()> {
        match self.leases.get(&lease.workspace_id) {
            Some(stored)
                if stored.token == lease.token
                    && stored.fencing == lease.fencing
                    && stored.expires_at > now =>
            {
                Ok(())
            }
            _ => Err(lost(&lease.workspace_id)),
        }
    }

    /// The leased workspace after a passing lease check.
    pub(super) fn fenced(&self, lease: &LeaseToken, now: i64) -> Outcome<Workspace> {
        self.check_lease(lease, now)?;
        self.workspace(&lease.workspace_id).cloned()
    }

    /// The oldest open (`Dispatching` or `Running`) operation of a workspace.
    pub(super) fn open_operation(&self, workspace: &Workspace) -> Outcome<()> {
        let open = self
            .operations
            .values()
            .filter(|op| op.workspace_id == workspace.id && op.phase.is_open())
            .min_by(|a, b| (a.started_at, &a.id).cmp(&(b.started_at, &b.id)));
        match open {
            Some(op) => Err(RepositoryError::OperationOpen {
                workspace_id: workspace.id.clone(),
                operation_id: op.id.clone(),
            }),
            None => Ok(()),
        }
    }

    /// The oldest `Uncertain` operation of the workspace's current
    /// generation.
    pub(super) fn uncertain_operation(&self, workspace: &Workspace) -> Outcome<()> {
        let uncertain = self
            .operations
            .values()
            .filter(|op| {
                op.workspace_id == workspace.id
                    && op.generation == workspace.generation
                    && op.phase == OperationPhase::Uncertain
            })
            .min_by(|a, b| (a.started_at, &a.id).cmp(&(b.started_at, &b.id)));
        match uncertain {
            Some(op) => Err(RepositoryError::OperationUncertain {
                workspace_id: workspace.id.clone(),
                operation_id: op.id.clone(),
            }),
            None => Ok(()),
        }
    }

    pub(super) fn store(&mut self, workspace: Workspace) -> Workspace {
        self.workspaces
            .insert(workspace.id.clone(), workspace.clone());
        workspace
    }

    pub(super) fn active_grant(&self, grant: &NetworkGrant) -> Option<&NetworkGrant> {
        self.grants.values().find(|stored| {
            stored.is_active()
                && stored.workspace_id == grant.workspace_id
                && stored.target == grant.target
        })
    }

    /// Validate a grant for insertion; `Ok(Some)` is the existing active
    /// grant for the same target.
    pub(super) fn check_grant(&self, grant: &NetworkGrant) -> Outcome<Option<NetworkGrant>> {
        if !grant.is_active() {
            return Err(invalid("a revoked grant cannot be inserted"));
        }
        self.workspace(&grant.workspace_id)?;
        if let Some(existing) = self.active_grant(grant) {
            return Ok(Some(existing.clone()));
        }
        if self.grants.contains_key(&grant.id) {
            return Err(invalid(format!("grant id {} is already used", grant.id)));
        }
        Ok(None)
    }

    pub(super) fn insert_grant(&mut self, grant: &NetworkGrant) -> Outcome<NetworkGrant> {
        if let Some(existing) = self.check_grant(grant)? {
            return Ok(existing);
        }
        self.grants.insert(grant.id.clone(), grant.clone());
        Ok(grant.clone())
    }

    pub(super) fn create(&mut self, new: &NewWorkspace) -> Outcome<Workspace> {
        if self.workspaces.contains_key(&new.id) {
            return Err(invalid(format!("workspace id {} is already used", new.id)));
        }
        let mut ids: Vec<&GrantId> = Vec::new();
        for grant in &new.initial_grants {
            if grant.workspace_id != new.id || !grant.is_active() {
                return Err(invalid(
                    "initial grants must be active grants of the new id",
                ));
            }
            if self.grants.contains_key(&grant.id) || ids.contains(&&grant.id) {
                return Err(invalid(format!("grant id {} is already used", grant.id)));
            }
            ids.push(&grant.id);
        }
        let workspace = self.store(Workspace {
            id: new.id.clone(),
            kind: new.kind,
            key: new.key.clone(),
            owner_scope: new.owner_scope.clone(),
            state: WorkspaceState::Ready,
            generation: Generation::FIRST,
            image_digest: new.image_digest.clone(),
            holds_user_files: false,
            created_at: new.now,
            last_used_at: new.now,
        });
        for grant in &new.initial_grants {
            self.insert_grant(grant)?;
        }
        Ok(workspace)
    }

    pub(super) fn insert_operation(
        &mut self,
        lease: &LeaseToken,
        op: &Operation,
        now: i64,
    ) -> Outcome<bool> {
        self.check_lease(lease, now)?;
        if let Some(stored) = self.operations.get(&op.id) {
            return if stored.same_identity(op) {
                Ok(false)
            } else {
                Err(RepositoryError::OperationConflict(op.id.clone()))
            };
        }
        if op.phase != OperationPhase::Dispatching || op.workspace_id != lease.workspace_id {
            return Err(invalid(
                "a new operation is Dispatching in the leased workspace",
            ));
        }
        let mut workspace = self.workspace(&lease.workspace_id)?.clone();
        if workspace.generation != op.generation {
            return Err(RepositoryError::StaleGeneration {
                workspace_id: workspace.id.clone(),
                expected: op.generation,
                current: workspace.generation,
            });
        }
        if workspace.state != WorkspaceState::Ready {
            return Err(conflict(&workspace));
        }
        workspace.state = WorkspaceState::Busy;
        workspace.last_used_at = workspace.last_used_at.max(now);
        self.store(workspace);
        self.operations.insert(op.id.clone(), op.clone());
        Ok(true)
    }

    pub(super) fn update_operation(
        &mut self,
        lease: &LeaseToken,
        id: &OperationId,
        update: &OperationUpdate,
        now: i64,
    ) -> Outcome<Operation> {
        self.check_lease(lease, now)?;
        let mut op = self
            .operations
            .get(id)
            .cloned()
            .ok_or_else(|| RepositoryError::OperationNotFound(id.clone()))?;
        if op.workspace_id != lease.workspace_id {
            return Err(lost(&lease.workspace_id));
        }
        if !op.phase.can_transition_to(&update.phase) {
            return Err(RepositoryError::OperationTransition {
                operation_id: id.clone(),
                from: op.phase,
                to: update.phase,
            });
        }
        op.phase = update.phase;
        op.ended_at = update.ended_at.or(op.ended_at);
        op.log_path = update.log_path.clone().or(op.log_path);
        op.truncated |= update.truncated;
        self.operations.insert(id.clone(), op.clone());
        Ok(op)
    }

    pub(super) fn acquire(
        &mut self,
        workspace_id: &WorkspaceId,
        holder: &LeaseHolder,
        token: &str,
        now: i64,
        ttl_ms: i64,
    ) -> Outcome<LeaseAcquire> {
        self.workspace(workspace_id)?;
        if let Some(stored) = self.leases.get(workspace_id)
            && stored.expires_at > now
        {
            return Ok(LeaseAcquire::Held {
                holder: stored.holder.clone(),
                expires_at: stored.expires_at,
            });
        }
        let expires_at = expiry(now, ttl_ms)?;
        let last = self.fencing.get(workspace_id).copied().unwrap_or(0);
        let fencing = last
            .checked_add(1)
            .ok_or_else(|| invalid("fencing number overflow"))?;
        self.fencing.insert(workspace_id.clone(), fencing);
        let stored = StoredLease {
            holder: holder.clone(),
            token: token.to_owned(),
            fencing,
            expires_at,
        };
        self.leases.insert(workspace_id.clone(), stored);
        Ok(LeaseAcquire::Acquired(LeaseToken {
            workspace_id: workspace_id.clone(),
            token: token.to_owned(),
            fencing,
            expires_at,
        }))
    }

    pub(super) fn record_denial(&mut self, denial: &DenialRecord) -> Outcome<NetworkRequest> {
        if denial.attempts == 0 {
            return Err(invalid("a denial stands for at least one attempt"));
        }
        self.workspace(&denial.workspace_id)?;
        let open = self.requests.values().find(|request| {
            request.state != RequestState::Approved
                && request.workspace_id == denial.workspace_id
                && request.target == denial.target
        });
        if let Some(request) = open {
            let mut request = request.clone();
            request.count = request
                .count
                .checked_add(denial.attempts)
                .ok_or_else(|| invalid("request count overflow"))?;
            request.operation_id = denial.operation_id.clone().or(request.operation_id);
            self.requests.insert(request.id.clone(), request.clone());
            return Ok(request);
        }
        if self.requests.contains_key(&denial.id) {
            return Err(invalid(format!("request id {} is already used", denial.id)));
        }
        let request = NetworkRequest {
            id: denial.id.clone(),
            workspace_id: denial.workspace_id.clone(),
            target: denial.target.clone(),
            operation_id: denial.operation_id.clone(),
            first_seen: denial.seen_at,
            count: denial.attempts,
            state: RequestState::Pending,
        };
        self.requests.insert(request.id.clone(), request.clone());
        Ok(request)
    }

    pub(super) fn decide(
        &mut self,
        id: &RequestId,
        decision: &RequestDecision,
    ) -> Outcome<RequestDecided> {
        let mut request = self
            .requests
            .get(id)
            .cloned()
            .ok_or_else(|| RepositoryError::RequestNotFound(id.clone()))?;
        if request.state != RequestState::Pending {
            return Err(RepositoryError::RequestNotPending {
                request_id: id.clone(),
                state: request.state,
            });
        }
        let grant = match decision {
            RequestDecision::Reject => {
                request.state = RequestState::Rejected;
                None
            }
            RequestDecision::Approve { grant } => {
                if grant.workspace_id != request.workspace_id || grant.target != request.target {
                    return Err(invalid(
                        "the grant must name the request's workspace and target",
                    ));
                }
                request.state = RequestState::Approved;
                Some(self.insert_grant(grant)?)
            }
        };
        self.requests.insert(id.clone(), request.clone());
        Ok(RequestDecided { request, grant })
    }
}
