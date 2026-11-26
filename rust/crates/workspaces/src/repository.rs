//! The repository port: workspaces, members, the operation projection,
//! network grants and requests, and the per-workspace lease. Implemented by
//! the SQLite store and by an in-memory twin in `agentkit-testkit`; one
//! conformance suite runs on both.
//!
//! # Fenced writes
//!
//! Every write that changes workspace or operation state takes a
//! [`LeaseToken`] and the caller's `now`. In the same transaction as the
//! write, the repository checks that the token is current for the
//! workspace: `token` and `fencing` equal the stored lease and the stored
//! lease's `expires_at > now` (the presented token's own `expires_at` is
//! never trusted). Otherwise it returns
//! [`RepositoryError::LeaseLost`] and changes nothing. A fenced method
//! that names no workspace acts on `lease.workspace_id`.
//!
//! Owner-side writes (members, grants, request decisions, denial records)
//! and `touch_workspace` are not fenced: they happen outside any run and
//! do not change what a writer may do inside the machine.
//!
//! # Workspace states
//!
//! `Ready -> Busy` only through [`WorkspaceRepository::insert_operation`];
//! `Busy -> Ready` through `mark_ready`, `advance_generation` or
//! `advance_and_unfence`; any live state `-> Fenced` through `fence`;
//! `Fenced -> Ready` through `unfence` or `advance_and_unfence`;
//! `Ready | Fenced -> Deleted` through `mark_deleted`. A refused
//! transition is [`RepositoryError::WorkspaceStateConflict`].
//!
//! A key is unique among records that are not `Deleted`: a deleted record
//! keeps its key for attribution, and the next use of the key creates a new
//! record with a new id.
//!
//! Time is always an argument; the repository never reads a clock.

use async_trait::async_trait;

use crate::error::RepositoryError;
use crate::ids::{AgentId, Generation, GrantId, OperationId, RequestId, WorkspaceId};
use crate::lease::{LeaseAcquire, LeaseHolder, LeaseInfo, LeaseToken};
use crate::network::{DenialRecord, NetworkGrant, NetworkRequest, RequestDecided, RequestDecision};
use crate::operation::{Operation, OperationUpdate};
use crate::workspace::{Member, NewWorkspace, Workspace, WorkspaceKey, WorkspaceUpsert};

/// Persistence for the workspaces context. See the module docs for the
/// fenced-write contract.
#[async_trait]
pub trait WorkspaceRepository: Send + Sync {
    // ---- workspaces -----------------------------------------------------

    /// Atomic create-or-get by `new.key` among records that are not
    /// `Deleted`. Concurrent first uses of one key yield exactly one record
    /// (F-17): the winner inserts the record at `Generation::FIRST`, state
    /// `Ready`, together with `new.initial_grants` in one transaction and
    /// gets `created: true`; every other caller gets the stored record and
    /// `created: false`, and its `id` and grants are ignored. A key whose
    /// only records are `Deleted` creates a new record. A winner whose
    /// grants are not active grants of `new.id`, or whose id or grant ids
    /// are taken, is [`RepositoryError::InvalidInput`] and creates nothing.
    async fn create_or_get_workspace(
        &self,
        new: &NewWorkspace,
    ) -> Result<WorkspaceUpsert, RepositoryError>;

    /// The record with this id.
    async fn workspace(&self, id: &WorkspaceId) -> Result<Option<Workspace>, RepositoryError>;

    /// The record with this key that is not `Deleted`.
    async fn workspace_by_key(
        &self,
        key: &WorkspaceKey,
    ) -> Result<Option<Workspace>, RepositoryError>;

    /// Every record, deleted ones included, ordered by `created_at` then id.
    /// Recovery reads it to find `Busy` workspaces.
    async fn list_workspaces(&self) -> Result<Vec<Workspace>, RepositoryError>;

    /// Unfenced: set `last_used_at` to `max(last_used_at, now)`. Unknown
    /// id: [`RepositoryError::WorkspaceNotFound`].
    async fn touch_workspace(&self, id: &WorkspaceId, now: i64) -> Result<(), RepositoryError>;

    /// Fenced: set `holds_user_files`. Refused on a deleted workspace.
    async fn set_holds_user_files(
        &self,
        lease: &LeaseToken,
        value: bool,
        now: i64,
    ) -> Result<Workspace, RepositoryError>;

    /// Fenced: `Busy -> Ready` (idempotent on `Ready`). Refused with
    /// [`RepositoryError::OperationOpen`] while an operation of the
    /// workspace is `Dispatching` or `Running`, then with
    /// [`RepositoryError::OperationUncertain`] while an operation of the
    /// workspace's current generation is `Uncertain` (the oldest one is
    /// named in both). A workspace whose last operation is uncertain
    /// therefore stays `Busy` until its machine is proven stopped and
    /// `advance_generation` moves it to `Ready` at the next generation;
    /// after that, the older generation's uncertain operations no longer
    /// block.
    async fn mark_ready(&self, lease: &LeaseToken, now: i64) -> Result<Workspace, RepositoryError>;

    /// Fenced: any live state `-> Fenced { reason, generation }`. The stored
    /// fence generation is the highest of `generation`, the current
    /// generation and, on a workspace already fenced, its fence generation
    /// (whose reason this replaces). Refused on a deleted workspace.
    async fn fence(
        &self,
        lease: &LeaseToken,
        reason: &str,
        generation: Generation,
        now: i64,
    ) -> Result<Workspace, RepositoryError>;

    /// Fenced: `Fenced -> Ready`, after an explicit recover proved the stop
    /// (INV-6). Refused from any other state and while an operation is open.
    async fn unfence(&self, lease: &LeaseToken, now: i64) -> Result<Workspace, RepositoryError>;

    /// Fenced compare-and-swap: when the stored generation equals
    /// `expected_current`, store `expected_current + 1` and `image_digest`,
    /// and turn `Busy` into `Ready` (`Fenced` stays fenced). Otherwise
    /// [`RepositoryError::StaleGeneration`]. The caller holds a
    /// `StopProof::Stopped` for `expected_current` or knows no machine was
    /// started (INV-5). Refused while an operation is open and on a
    /// deleted workspace; allowed while an operation of `expected_current`
    /// is `Uncertain` (that is the recovery path).
    async fn advance_generation(
        &self,
        lease: &LeaseToken,
        expected_current: Generation,
        image_digest: &str,
        now: i64,
    ) -> Result<Workspace, RepositoryError>;

    /// Fenced compare-and-swap that recovers in one transaction: when the
    /// stored generation equals `expected_current`, store `next` and
    /// `image_digest` and make the workspace `Ready` from `Busy`, `Fenced`
    /// or `Ready`. `next` must be above `expected_current` and above the
    /// fence generation of a fenced workspace, else
    /// [`RepositoryError::InvalidInput`]; another stored generation is
    /// [`RepositoryError::StaleGeneration`]. Refused while an operation is
    /// open and on a deleted workspace. The caller holds a
    /// `StopProof::Stopped` for every generation below `next` that may have
    /// run (INV-5, INV-6): a crash can no longer leave the generation
    /// advanced and the fence in place.
    async fn advance_and_unfence(
        &self,
        lease: &LeaseToken,
        expected_current: Generation,
        next: Generation,
        image_digest: &str,
        now: i64,
    ) -> Result<Workspace, RepositoryError>;

    /// Fenced: `Ready | Fenced -> Deleted`, after the stop was proven and
    /// storage destroyed. Refused on `Busy`.
    async fn mark_deleted(
        &self,
        lease: &LeaseToken,
        now: i64,
    ) -> Result<Workspace, RepositoryError>;

    // ---- lease ----------------------------------------------------------

    /// Take the workspace lease for `ttl_ms` when no unexpired lease exists.
    /// The new lease gets the caller's `token` and a fencing number above
    /// every earlier one of this workspace. An unexpired lease (the same
    /// holder's included) answers `Held`. Unknown workspace:
    /// [`RepositoryError::WorkspaceNotFound`]; a negative `ttl_ms`:
    /// [`RepositoryError::InvalidInput`].
    async fn acquire_lease(
        &self,
        workspace_id: &WorkspaceId,
        holder: &LeaseHolder,
        token: &str,
        now: i64,
        ttl_ms: i64,
    ) -> Result<LeaseAcquire, RepositoryError>;

    /// Extend a current lease to `now + ttl_ms`. A stale or expired token is
    /// [`RepositoryError::LeaseLost`].
    async fn renew_lease(
        &self,
        lease: &LeaseToken,
        now: i64,
        ttl_ms: i64,
    ) -> Result<LeaseToken, RepositoryError>;

    /// Drop the lease when `token` and `fencing` match the stored one,
    /// expired or not. Otherwise [`RepositoryError::LeaseLost`] and nothing
    /// changes.
    async fn release_lease(&self, lease: &LeaseToken) -> Result<(), RepositoryError>;

    /// The unexpired lease of a workspace, if any (who holds it).
    async fn lease(
        &self,
        workspace_id: &WorkspaceId,
        now: i64,
    ) -> Result<Option<LeaseInfo>, RepositoryError>;

    /// Every stored lease, expired ones included. Recovery reads it at
    /// start.
    async fn list_leases(&self) -> Result<Vec<LeaseInfo>, RepositoryError>;

    /// Recovery only, after the host knows the holder is gone (process
    /// restart): drop the lease when its fencing number equals `fencing`,
    /// expired or not. Returns whether a lease was dropped. The fencing
    /// number keeps increasing, so the broken token never becomes current.
    /// An unknown workspace answers `false`.
    async fn break_lease(
        &self,
        workspace_id: &WorkspaceId,
        fencing: u64,
    ) -> Result<bool, RepositoryError>;

    // ---- members (unfenced) ---------------------------------------------

    /// Add a member; an existing membership is returned unchanged. Unknown
    /// workspace: [`RepositoryError::WorkspaceNotFound`].
    async fn add_member(&self, member: &Member) -> Result<Member, RepositoryError>;

    /// Remove a member. Returns whether one was removed. Affects the next
    /// admission, never a running one.
    async fn remove_member(
        &self,
        workspace_id: &WorkspaceId,
        agent_id: &AgentId,
    ) -> Result<bool, RepositoryError>;

    /// One membership.
    async fn member(
        &self,
        workspace_id: &WorkspaceId,
        agent_id: &AgentId,
    ) -> Result<Option<Member>, RepositoryError>;

    /// Members ordered by `added_at` then agent id.
    async fn list_members(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<Vec<Member>, RepositoryError>;

    // ---- operations (fenced writes) -------------------------------------

    /// Fenced: write the projection row before dispatch. Checks run in this
    /// order and the first failing one answers:
    /// 1. the lease ([`RepositoryError::LeaseLost`]);
    /// 2. an existing row with this id: the same identity
    ///    ([`Operation::same_identity`], the argument hash included) answers
    ///    `Ok(false)` and changes
    ///    nothing, and the caller must not dispatch again (INV-3); other
    ///    identity is [`RepositoryError::OperationConflict`];
    /// 3. phase `Dispatching` and `workspace_id == lease.workspace_id`
    ///    ([`RepositoryError::InvalidInput`]);
    /// 4. the workspace's current generation
    ///    ([`RepositoryError::StaleGeneration`]);
    /// 5. a `Ready` workspace ([`RepositoryError::WorkspaceStateConflict`]).
    ///
    /// Inserting turns the workspace `Busy` and sets `last_used_at` to
    /// `max(last_used_at, now)`, so at most one operation per workspace is
    /// unsettled (INV-4). Answers `Ok(true)`.
    async fn insert_operation(
        &self,
        lease: &LeaseToken,
        op: &Operation,
        now: i64,
    ) -> Result<bool, RepositoryError>;

    /// Fenced: change an operation's phase and output fields. The lease must
    /// be for the operation's workspace ([`RepositoryError::LeaseLost`]).
    /// The phase change must pass `OperationPhase::can_transition_to`
    /// ([`RepositoryError::OperationTransition`]): a settled phase is
    /// written once (INV-7) and `Uncertain` may only be settled by a late
    /// receipt. `ended_at` and `log_path` overwrite only when `Some`;
    /// `truncated` only ever turns on. Never changes the workspace state.
    /// Returns the stored row.
    async fn update_operation(
        &self,
        lease: &LeaseToken,
        id: &OperationId,
        update: &OperationUpdate,
        now: i64,
    ) -> Result<Operation, RepositoryError>;

    /// One operation.
    async fn operation(&self, id: &OperationId) -> Result<Option<Operation>, RepositoryError>;

    /// The newest `limit` operations of a workspace, by `started_at`
    /// descending then id descending.
    async fn list_operations(
        &self,
        workspace_id: &WorkspaceId,
        limit: usize,
    ) -> Result<Vec<Operation>, RepositoryError>;

    /// Every `Dispatching` or `Running` operation of every workspace, by
    /// `started_at` then id. Recovery reconciles each one.
    async fn list_open_operations(&self) -> Result<Vec<Operation>, RepositoryError>;

    // ---- network grants (unfenced) --------------------------------------

    /// Insert an active grant. When an active grant for the same workspace,
    /// host and port exists, it is returned and nothing is inserted. A
    /// revoked input or an unknown workspace is refused.
    async fn insert_grant(&self, grant: &NetworkGrant) -> Result<NetworkGrant, RepositoryError>;

    /// Revoke a grant at `now`. Idempotent: an already revoked grant keeps
    /// its first `revoked_at`. Returns the stored row.
    async fn revoke_grant(&self, id: &GrantId, now: i64) -> Result<NetworkGrant, RepositoryError>;

    /// Active grants of a workspace, by `granted_at` then id.
    async fn list_active_grants(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<Vec<NetworkGrant>, RepositoryError>;

    // ---- network requests (unfenced) ------------------------------------

    /// Upsert-increment by `(workspace, host, port)` among requests that
    /// are not `Approved` (a target has at most one `Pending` or `Rejected`
    /// request). A pending one gains `attempts`. A rejected one gains
    /// `attempts` and stays `Rejected`: the owner is not asked again, and
    /// can still grant the target with `insert_grant`. Either takes the
    /// denial's operation id when it names one. With neither (none yet, or
    /// every earlier one approved) a new `Pending` request is inserted with
    /// `denial.id`, `first_seen = seen_at` and `count = attempts`. The
    /// returned request's `state` says which happened. `attempts == 0` is
    /// [`RepositoryError::InvalidInput`]; an unknown workspace is
    /// [`RepositoryError::WorkspaceNotFound`].
    async fn record_denial(&self, denial: &DenialRecord)
    -> Result<NetworkRequest, RepositoryError>;

    /// Decide a pending request. `Approve` inserts its grant in the same
    /// transaction (an existing active grant for the target is reused); the
    /// grant must name the request's workspace and target and be active,
    /// else [`RepositoryError::InvalidInput`] and nothing changes. A decided
    /// request is [`RepositoryError::RequestNotPending`].
    async fn decide_request(
        &self,
        id: &RequestId,
        decision: &RequestDecision,
    ) -> Result<RequestDecided, RepositoryError>;

    /// Pending requests of a workspace, by `first_seen` then id.
    async fn list_pending_requests(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<Vec<NetworkRequest>, RepositoryError>;
}
