//! Binding-store crash-safety, create-or-adopt across client recreation,
//! receipt recording, restart decisions, and reconcile classification —
//! all against the real worker binary with the fake backend.

mod support;

use std::sync::Arc;

use agentkit_sandbox_host::binding::{
    BindingError, BindingMode, BindingState, FileStore, InMemoryStore, RestartDecision,
    SandboxBinding, SandboxBindingStore, destroy_and_record, ensure_sandbox, restart_decision,
    stop_and_record,
};
use agentkit_sandbox_host::contracts::api::VmPhysState;
use agentkit_sandbox_host::contracts::api::{SandboxManager, SandboxWorkspaceIo};
use agentkit_sandbox_host::contracts::error::SandboxErrorCode;
use agentkit_sandbox_host::contracts::policy::SandboxCapability;
use agentkit_sandbox_host::reconcile::reconcile;
use base64::Engine as _;

use support::{TestEnv, gp, manage_ctx, now_ms, scaffold_grant, spec};

fn binding_for(tenant: &str, session: &str, sandbox: &str) -> SandboxBinding {
    SandboxBinding::new_active(
        support::tid(tenant),
        support::prid(tenant),
        support::sess(session),
        support::sid(sandbox),
        BindingMode::Ephemeral,
        "agentkit/coding@1",
        support::image_digest(),
        agentkit_sandbox_host::contracts::ids::PolicyGeneration(1),
        support::policy_digest(),
        support::genof(1),
        now_ms(),
    )
}

#[tokio::test]
async fn file_store_atomicity_and_reload() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("state/bindings.json");

    let store = FileStore::open(&path).expect("open fresh");
    store
        .put(&binding_for("alice", "s1", "sb-a"))
        .expect("put 1");
    store
        .put(&binding_for("alice", "s2", "sb-b"))
        .expect("put 2");

    // Published file is complete JSON on every read (temp+rename).
    let bytes = std::fs::read(&path).expect("file");
    let doc: serde_json::Value = serde_json::from_slice(&bytes).expect("complete json");
    assert_eq!(
        doc["schema_version"].as_u64(),
        Some(u64::from(
            agentkit_sandbox_host::binding::BINDING_SCHEMA_VERSION
        ))
    );
    assert_eq!(doc["bindings"].as_array().map(Vec::len), Some(2));
    // No temp litter survives a successful write.
    let leftovers: Vec<_> = std::fs::read_dir(tmp.path().join("state"))
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with(".bindings.json.tmp-")
        })
        .collect();
    assert!(leftovers.is_empty(), "temp litter: {leftovers:?}");

    // Reload sees everything (adoption across host restart).
    let reopened = FileStore::open(&path).expect("reopen");
    assert_eq!(
        reopened
            .get(&support::tid("alice"), &support::sid("sb-a"))
            .expect("get")
            .map(|b| b.sandbox_id.as_str().to_string()),
        Some("sb-a".into())
    );

    // A torn/garbage file fails closed.
    std::fs::write(&path, b"{\"schema_version\":1,\"binding").unwrap();
    assert!(matches!(
        FileStore::open(&path),
        Err(BindingError::Corrupt(_))
    ));
    // A future schema version is refused, never best-effort parsed.
    std::fs::write(&path, b"{\"schema_version\":9,\"bindings\":[]}").unwrap();
    assert!(matches!(
        FileStore::open(&path),
        Err(BindingError::Schema { found: 9, .. })
    ));

    // Concurrent writers all land.
    std::fs::write(&path, b"{\"schema_version\":1,\"bindings\":[]}").unwrap();
    let store = Arc::new(FileStore::open(&path).expect("reset"));
    let mut tasks = Vec::new();
    for i in 0..6 {
        let s = Arc::clone(&store);
        tasks.push(tokio::task::spawn_blocking(move || {
            s.put(&binding_for("alice", "conc", &format!("sb-c{i}")))
                .expect("put");
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    let reloaded = FileStore::open(&path).expect("reload");
    assert_eq!(
        reloaded
            .list()
            .expect("list")
            .iter()
            .filter(|b| b.session_id.as_str() == "conc")
            .count(),
        6
    );

    // Wrong-schema record is refused at put time too.
    let mut bad = binding_for("alice", "s9", "sb-9");
    bad.schema_version = 7;
    assert!(matches!(
        store.put(&bad),
        Err(BindingError::Schema { found: 7, .. })
    ));
}

#[tokio::test]
async fn ensure_sandbox_adopts_across_client_recreation() {
    let env = TestEnv::start().await;
    let store = InMemoryStore::new();
    let sbx = support::deterministic_sandbox("alice", "sess-adopt");
    let client1 = env
        .client("alice", "alice", "sess-adopt", sbx.as_str())
        .await;

    let (binding1, sref) = ensure_sandbox(&client1, &store, &spec(), "agentkit/coding@1", now_ms())
        .await
        .expect("first create");
    assert_eq!(binding1.state, BindingState::Active);
    assert_eq!(sref.sandbox_id, sbx);

    // Recreated client (fresh connection, fresh instance): adopt, do not
    // recreate — created_at_ms is the stored one, and the guest data from
    // the first incarnation survives.
    let task = agentkit_sandbox_contracts::ids::TaskId::new("task-adopt").unwrap();
    let lease = client1
        .acquire_writer(&manage_ctx("alice", "alice", "sess-adopt"), &task, 600_000)
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

    let client2 = env
        .client("alice", "alice", "sess-adopt", sbx.as_str())
        .await;
    let (binding2, _) = ensure_sandbox(&client2, &store, &spec(), "agentkit/coding@1", now_ms())
        .await
        .expect("adopt");
    assert_eq!(binding2.created_at_ms, binding1.created_at_ms);
    let rg = scaffold_grant(client2.binding(), SandboxCapability::SandboxRead);
    let reply = client2
        .read(&rg, &gp("/workspace/keep.txt"), None, None)
        .await
        .expect("adopted sandbox serves workspace io");
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(&reply.data_b64)
            .unwrap(),
        b"kept"
    );

    // Uncertain bindings gate further effects until an operator clears them.
    let mut gated = binding2.clone();
    gated.operator_reconcile_required = true;
    gated.quarantine_reason = Some("simulated".into());
    store.put(&gated).unwrap();
    let err = ensure_sandbox(&client2, &store, &spec(), "agentkit/coding@1", now_ms())
        .await
        .expect_err("uncertain binding gates effects");
    assert_eq!(err.code, SandboxErrorCode::Quarantined);
}

#[tokio::test]
async fn stop_destroy_receipts_record_truthfully() {
    let env = TestEnv::start().await;
    let store = InMemoryStore::new();
    let sbx = support::deterministic_sandbox("alice", "sess-rct");
    let client = env.client("alice", "alice", "sess-rct", sbx.as_str()).await;
    ensure_sandbox(&client, &store, &spec(), "agentkit/coding@1", now_ms())
        .await
        .expect("create");

    let stopped = stop_and_record(&client, &store).await.expect("stop");
    assert_eq!(stopped.state, BindingState::Stopped);
    assert!(
        !stopped.operator_reconcile_required,
        "conclusive fake-backend stop needs no operator"
    );

    let destroyed = destroy_and_record(&client, &store).await.expect("destroy");
    assert_eq!(destroyed.state, BindingState::Destroyed);
    let receipt = destroyed.last_receipt.expect("receipt recorded");
    assert!(
        receipt.storage_deleted,
        "fake backend deletes storage: {receipt:?}"
    );
    assert!(receipt.reservations_released);
    assert!(receipt.termination.conclusive);

    // Destroyed bindings do not block a fresh session sandbox under the
    // same session key: ensure_sandbox creates anew (new created_at).
    let (fresh, _) = ensure_sandbox(&client, &store, &spec(), "agentkit/coding@1", now_ms() + 5)
        .await
        .expect("recreate after destroy");
    assert_eq!(fresh.state, BindingState::Active);
    assert!(fresh.created_at_ms > destroyed.created_at_ms);
}

#[tokio::test]
async fn restart_decision_adopt_and_uncertain() {
    let env = TestEnv::start().await;
    let store = InMemoryStore::new();
    let sbx = support::deterministic_sandbox("alice", "sess-rst");
    let client = env.client("alice", "alice", "sess-rst", sbx.as_str()).await;
    ensure_sandbox(&client, &store, &spec(), "agentkit/coding@1", now_ms())
        .await
        .expect("create");

    // Up-to-date journal knowledge + Running VM => adopt.
    let inv = client
        .reconcile_query(&manage_ctx("alice", "alice", "sess-rst"))
        .await
        .expect("inventory");
    let seq = inv
        .sandboxes
        .iter()
        .find(|s| s.sandbox_id == sbx)
        .expect("listed")
        .journal_seq;
    let mut b = store
        .get(&support::tid("alice"), &sbx)
        .unwrap()
        .expect("binding");
    b.last_journal_seq = seq;
    store.put(&b).unwrap();
    let decision = restart_decision(&client, &store).await.expect("decision");
    assert!(
        matches!(
            decision,
            RestartDecision::Adopted {
                physical: VmPhysState::Running
            }
        ),
        "adopt expected, got {decision:?}"
    );

    // Pending journal gap under a Running claim => uncertain + operator flag.
    let mut b = store
        .get(&support::tid("alice"), &sbx)
        .unwrap()
        .expect("binding");
    b.last_journal_seq = 0;
    store.put(&b).unwrap();
    let decision = restart_decision(&client, &store).await.expect("decision");
    match decision {
        RestartDecision::Uncertain { reason } => {
            assert!(reason.contains("pending journal gap"), "{reason}");
        }
        other => panic!("uncertain expected, got {other:?}"),
    }
    let b = store
        .get(&support::tid("alice"), &sbx)
        .unwrap()
        .expect("binding");
    assert_eq!(b.state, BindingState::Uncertain);
    assert!(b.operator_reconcile_required);
}

#[tokio::test]
async fn reconcile_classifies_adopted_quarantined_orphaned_uncertain() {
    let mut env = TestEnv::start().await;
    let store = InMemoryStore::new();

    // (1) adopted: bound + live + conclusive.
    let sbx = support::deterministic_sandbox("alice", "sess-rec");
    let client = env.client("alice", "alice", "sess-rec", sbx.as_str()).await;
    ensure_sandbox(&client, &store, &spec(), "agentkit/coding@1", now_ms())
        .await
        .expect("create");

    // (2) orphaned: sandbox on the worker with no binding at all.
    let sb_orphan = support::deterministic_sandbox("carol", "sess-c");
    let carol = env
        .client("carol", "carol", "sess-c", sb_orphan.as_str())
        .await;
    carol
        .create(&manage_ctx("carol", "carol", "sess-c"), &spec())
        .await
        .expect("unbound create");

    // (3) uncertain: live binding the worker has never seen.
    let ghost = support::deterministic_sandbox("dave", "sess-d");
    store
        .put(&binding_for("dave", "sess-d", ghost.as_str()))
        .expect("ghost binding");

    let report = reconcile(&client, &store).await.expect("reconcile");
    assert!(
        report.adopted.contains(&sbx),
        "adopted: {:?}",
        report.adopted
    );
    assert!(
        report.orphaned.contains(&sb_orphan),
        "orphaned: {:?}",
        report.orphaned
    );
    assert!(
        report.uncertain.iter().any(|(s, _)| *s == ghost),
        "uncertain: {:?}",
        report.uncertain
    );

    // (4) quarantined: worker restart leaves launched VMs occupied-unknown;
    // a client that saw its connection die biases reconcile to quarantine.
    env.restart_worker().await;
    let report = reconcile(&client, &store)
        .await
        .expect("reconcile post-restart");
    assert!(
        report.quarantined.iter().any(|(s, _)| *s == sbx),
        "quarantined: {:?}",
        report.quarantined
    );
    let b = store
        .get(&support::tid("alice"), &sbx)
        .unwrap()
        .expect("binding");
    assert!(b.operator_reconcile_required);
    assert!(
        b.quarantine_reason
            .as_deref()
            .unwrap()
            .contains("unverified")
    );
}

/// §14.4 upgrade path: a v1 store file (pre-subagent, no `children`
/// field) opens and reads with the v2 reader. New writes stamp v2.
#[tokio::test]
async fn legacy_v1_binding_file_upgrades_on_open() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("state/bindings.json");
    {
        let store = FileStore::open(&path).expect("create");
        store
            .put(&binding_for("alice", "s-legacy", "sb-legacy"))
            .expect("put");
    }
    // Downgrade the file to the v1 shape: schema 1, no children keys.
    let mut doc: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    doc["schema_version"] = serde_json::json!(1);
    for b in doc["bindings"].as_array_mut().unwrap() {
        b.as_object_mut().unwrap().remove("children");
    }
    std::fs::write(&path, serde_json::to_vec_pretty(&doc).unwrap()).unwrap();

    let reopened = FileStore::open(&path).expect("v1 file must open with v2 reader");
    let b = reopened
        .get(&support::tid("alice"), &support::sid("sb-legacy"))
        .expect("read")
        .expect("binding survived");
    assert!(b.children.is_empty());

    // A newer-than-this-build file still refuses (no best-effort downgrade).
    let bad = tmp.path().join("state/future.json");
    std::fs::write(&bad, br#"{"schema_version": 99, "bindings": []}"#).unwrap();
    assert!(FileStore::open(&bad).is_err(), "future schema must refuse");
}
