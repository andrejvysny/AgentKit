//! The SQLite adapter of the workspaces context's repository port (ADR
//! 0038): the suite `agentkit-testkit` runs against its in-memory twin runs
//! here too, plus what only a file can show (data survives a reopen, and a
//! version-22 database gains the sandbox tables on open).

use agentkit_store::{SqliteStore, SqliteWorkspaceRepository};
use agentkit_testkit::conformance::workspace_repository;
use agentkit_workspaces::{
    AgentId, DenialRecord, Generation, GrantId, GrantSource, HostPort, LeaseAcquire, LeaseHolder,
    Member, MemberRole, NetworkGrant, NewWorkspace, Operation, OperationId, OperationKindTag,
    OperationPhase, RequestId, RequestState, Sha256, WorkspaceId, WorkspaceKey, WorkspaceKind,
    WorkspaceRepository, WorkspaceState,
};
use std::path::{Path, PathBuf};

const SANDBOX_TABLES: [&str; 6] = [
    "sandbox_network_request",
    "sandbox_network_grant",
    "sandbox_operation",
    "sandbox_member",
    "sandbox_workspace_lease",
    "sandbox_workspace",
];

#[tokio::test]
async fn sqlite_workspace_repository_conforms() {
    workspace_repository::run(|| async {
        SqliteWorkspaceRepository::new(SqliteStore::memory(16).expect("store"))
    })
    .await;
}

fn file_store(directory: &Path) -> (SqliteStore, PathBuf) {
    let path = directory.join("sandbox.db");
    (SqliteStore::open(&path, 16).expect("open"), path)
}

fn new_workspace() -> NewWorkspace {
    let id = WorkspaceId::new("ws-1");
    NewWorkspace {
        initial_grants: vec![NetworkGrant {
            id: GrantId::new("g-1"),
            workspace_id: id.clone(),
            target: HostPort::new("pypi.org", 443).expect("target"),
            source: GrantSource::Default,
            granted_at: 100,
            revoked_at: None,
        }],
        id,
        kind: WorkspaceKind::ProjectChats,
        key: WorkspaceKey::new("chats:project:p1"),
        owner_scope: "owner:1".into(),
        image_digest: "sha256:image-1".into(),
        now: 100,
    }
}

fn holder() -> LeaseHolder {
    LeaseHolder {
        agent_id: AgentId::new("a1"),
        run_id: "run-1".into(),
        label: "Researcher".into(),
        host_id: "host-1".into(),
        boot_id: "boot-1".into(),
    }
}

/// A workspace with a member, a lease, a running operation, a grant and a
/// pending request.
async fn seed(repo: &SqliteWorkspaceRepository) -> Operation {
    let ws = port(repo.create_or_get_workspace(&new_workspace()).await).workspace;
    let member = Member {
        workspace_id: ws.id.clone(),
        agent_id: AgentId::new("a1"),
        role: MemberRole::Member,
        added_at: 110,
    };
    port(repo.add_member(&member).await);
    let LeaseAcquire::Acquired(lease) = port(
        repo.acquire_lease(&ws.id, &holder(), "token-1", 1_000, 60_000)
            .await,
    ) else {
        panic!("lease held");
    };
    let op = Operation {
        id: OperationId::new("op-1"),
        workspace_id: ws.id.clone(),
        generation: ws.generation,
        kind: OperationKindTag::Exec,
        actor_agent_id: AgentId::new("a1"),
        run_id: "run-1".into(),
        phase: OperationPhase::Dispatching,
        started_at: 1_100,
        ended_at: None,
        log_path: None,
        truncated: false,
        image_digest: ws.image_digest.clone(),
        summary: "pip install requests".into(),
        args_sha256: Sha256::of(b"pip install requests"),
    };
    assert!(port(repo.insert_operation(&lease, &op, 1_100).await));
    let denial = DenialRecord {
        id: RequestId::new("r-1"),
        workspace_id: ws.id.clone(),
        target: HostPort::new("example.com", 443).expect("target"),
        operation_id: Some(op.id.clone()),
        seen_at: 1_200,
        attempts: 2,
    };
    port(repo.record_denial(&denial).await);
    op
}

fn port<T, E: std::fmt::Debug>(outcome: Result<T, E>) -> T {
    outcome.expect("port call")
}

async fn assert_seeded(repo: &SqliteWorkspaceRepository, op: &Operation) {
    let ws = port(
        repo.workspace_by_key(&WorkspaceKey::new("chats:project:p1"))
            .await,
    )
    .expect("workspace kept");
    assert_eq!(
        (ws.id.as_str(), ws.state, ws.generation),
        ("ws-1", WorkspaceState::Busy, Generation::FIRST)
    );
    assert_eq!(
        (ws.kind, ws.last_used_at),
        (WorkspaceKind::ProjectChats, 1_100)
    );
    assert_eq!(port(repo.list_members(&ws.id).await).len(), 1);
    assert_eq!(port(repo.list_open_operations().await), vec![op.clone()]);
    assert_eq!(
        port(repo.list_active_grants(&ws.id).await),
        new_workspace().initial_grants
    );
    let pending = port(repo.list_pending_requests(&ws.id).await);
    assert_eq!(pending.len(), 1);
    assert_eq!(
        (pending[0].count, pending[0].state),
        (2, RequestState::Pending)
    );
    let lease = port(repo.lease(&ws.id, 2_000).await).expect("lease kept");
    assert_eq!((lease.holder, lease.expires_at), (holder(), 61_000));
}

#[tokio::test]
async fn reopening_the_file_keeps_every_record() {
    let directory = tempfile::tempdir().expect("tempdir");
    let (store, path) = file_store(directory.path());
    let op = seed(&SqliteWorkspaceRepository::new(store.clone())).await;
    drop(store);
    let reopened = SqliteStore::open(&path, 16).expect("reopen");
    let repo = SqliteWorkspaceRepository::new(reopened);
    assert_seeded(&repo, &op).await;
    // The fencing number keeps counting from the stored one.
    let ws = WorkspaceId::new("ws-1");
    let before = port(repo.list_leases().await)[0].fencing;
    assert!(port(repo.break_lease(&ws, before).await));
    match port(
        repo.acquire_lease(&ws, &holder(), "token-2", 2_000, 1_000)
            .await,
    ) {
        LeaseAcquire::Acquired(lease) => assert!(lease.fencing > before),
        held => panic!("expected the lease, got {held:?}"),
    }
}

#[tokio::test]
async fn a_version_22_database_gains_the_sandbox_tables_on_open() {
    let directory = tempfile::tempdir().expect("tempdir");
    let (store, path) = file_store(directory.path());
    store
        .create_chat("chat-1", None, serde_json::json!({}), 1_000)
        .await
        .expect("chat");
    drop(store);
    {
        let raw = rusqlite::Connection::open(&path).expect("raw connection");
        for table in SANDBOX_TABLES {
            raw.execute_batch(&format!("DROP TABLE {table};"))
                .expect("drop");
        }
        raw.pragma_update(None, "user_version", 22)
            .expect("downgrade");
    }
    let store = SqliteStore::open(&path, 16).expect("migrate");
    assert_eq!(store.list_chats(true).await.expect("chats").len(), 1);
    let op = seed(&SqliteWorkspaceRepository::new(store.clone())).await;
    assert_seeded(&SqliteWorkspaceRepository::new(store.clone()), &op).await;
    drop(store);
    let raw = rusqlite::Connection::open(&path).expect("raw connection");
    let version: i64 = raw
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("version");
    assert_eq!(version, agentkit_store::SCHEMA_VERSION);
}
