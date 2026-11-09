//! SQLite host store parity with the in-memory/file stores plus the fenced
//! reservation ledger driving create→destroy accounting (§5.4, M3).

mod support;

use agentkit_sandbox_host::binding::{
    BindingState, SandboxBinding, SandboxBindingStore, ensure_sandbox, stop_and_record,
};
use agentkit_sandbox_host::sqlite_store::{LedgerError, SqliteStore};
use agentkit_sandbox_worker::registry::QuotaCeilings;

use support::{TestEnv, gp, now_ms, scaffold_grant, spec};

use agentkit_sandbox_host::contracts::api::SandboxWorkspaceIo;
use agentkit_sandbox_host::contracts::policy::SandboxCapability;

#[tokio::test]
async fn sqlite_store_adopts_bindings_across_client_recreation() {
    let env = TestEnv::start().await;
    let tmp = tempfile::tempdir().unwrap();
    let store = SqliteStore::open(&tmp.path().join("sandbox.db")).expect("open");
    let sbx = support::deterministic_sandbox("alice", "sess-sqlite");
    let client1 = env
        .client("alice", "alice", "sess-sqlite", sbx.as_str())
        .await;
    let (b1, _) = ensure_sandbox(&client1, &store, &spec(), "agentkit/coding@1", now_ms())
        .await
        .expect("create");
    assert_eq!(b1.state, BindingState::Active);

    let task = agentkit_sandbox_contracts::ids::TaskId::new("task-sqlite").unwrap();
    let lease = client1
        .acquire_writer(
            &support::manage_ctx("alice", "alice", "sess-sqlite"),
            &task,
            600_000,
        )
        .await
        .expect("writer admission");
    let wg = support::scaffold_write_grant(
        client1.binding(),
        SandboxCapability::SandboxWrite,
        &task,
        &lease,
    );
    client1
        .write(&wg, &gp("/workspace/keep.txt"), b"kept".to_vec(), None)
        .await
        .unwrap();

    // Reopen the store (host restart) and recreate the client: adopt.
    drop(store);
    let store = SqliteStore::open(&tmp.path().join("sandbox.db")).expect("reopen");
    let client2 = env
        .client("alice", "alice", "sess-sqlite", sbx.as_str())
        .await;
    let (b2, _) = ensure_sandbox(&client2, &store, &spec(), "agentkit/coding@1", now_ms())
        .await
        .expect("adopt");
    assert_eq!(b2.created_at_ms, b1.created_at_ms);
    let rg = scaffold_grant(client2.binding(), SandboxCapability::SandboxRead);
    let reply = client2
        .read(&rg, &gp("/workspace/keep.txt"), None, None)
        .await
        .expect("io on adopted sandbox");
    use base64::Engine as _;
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(&reply.data_b64)
            .unwrap(),
        b"kept"
    );
}

#[tokio::test]
async fn ledger_tracks_binding_lifecycle_end_to_end() {
    let env = TestEnv::start().await;
    let tmp = tempfile::tempdir().unwrap();
    let store = SqliteStore::open(&tmp.path().join("sandbox.db")).expect("open");
    let tenant = support::tid("alice");
    let sbx = support::deterministic_sandbox("alice", "sess-ledger");
    let client = env
        .client("alice", "alice", "sess-ledger", sbx.as_str())
        .await;

    // Capacity is reserved before the launch effect and the binding is
    // written after — the ledger refuses to overcommit either way.
    store
        .set_ceilings(
            &tenant,
            &QuotaCeilings {
                max_live_vms_per_tenant: 1,
                ..QuotaCeilings::default()
            },
        )
        .unwrap();
    let ticket = store
        .reserve(&tenant, &sbx, support::genof(1), 1, 256, 1024)
        .expect("reserve");
    let (binding, _) = ensure_sandbox(&client, &store, &spec(), "agentkit/coding@1", now_ms())
        .await
        .expect("create");
    assert_eq!(binding.state, BindingState::Active);

    // A second tenant sandbox would exceed the single-VM ceiling.
    let other = support::sid("sbx-other");
    assert!(matches!(
        store.reserve(&tenant, &other, support::genof(1), 1, 16, 16),
        Err(LedgerError::Exceeded(_))
    ));

    // Stop records truthfully; release is fenced by the grant ticket.
    let stopped = stop_and_record(&client, &store).await.expect("stop");
    assert_eq!(stopped.state, BindingState::Stopped);
    store.release(&ticket).expect("fenced release");
    assert_eq!(store.usage(&tenant).unwrap().live_vms, 0);

    // The binding survives as an audit record.
    assert!(store.get(&tenant, &sbx).unwrap().is_some());
}
