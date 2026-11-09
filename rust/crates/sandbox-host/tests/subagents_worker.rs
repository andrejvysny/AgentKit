//! M6 subagent integration against the live worker: parent/child writer
//! contention resolves deterministically through the worker's single
//! writer admission (S09/T16), the host gate refuses over-reach for
//! children (S15), and parent cancellation stops child grants (M6).

mod support;

use base64::Engine as _;

use agentkit_sandbox_host::binding::{InMemoryStore, SandboxBindingStore, ensure_sandbox};
use agentkit_sandbox_host::contracts::api::SandboxWorkspaceIo;
use agentkit_sandbox_host::contracts::grant::WorkspaceGrant;
use agentkit_sandbox_host::contracts::ids::TaskId;
use agentkit_sandbox_host::contracts::policy::SandboxCapability;
use agentkit_sandbox_host::subagent;

use support::{gp, manage_ctx, now_ms, scaffold_grant, scaffold_write_grant, spec};

fn task(s: &str) -> TaskId {
    TaskId::new(s).unwrap()
}

fn write_grant(
    client: &agentkit_sandbox_host::client::WorkerClient,
    capability: SandboxCapability,
    t: &TaskId,
    lease: &agentkit_sandbox_host::contracts::grant::WriterLeaseToken,
) -> WorkspaceGrant {
    scaffold_write_grant(client.binding(), capability, t, lease)
}

#[tokio::test]
async fn parent_child_writer_contention_is_deterministic() {
    let env = support::TestEnv::start().await;
    let store = InMemoryStore::new();
    let sbx = support::deterministic_sandbox("alice", "sess-w");
    let client = env.client("alice", "alice", "sess-w", sbx.as_str()).await;
    let ctx = manage_ctx("alice", "alice", "sess-w");
    ensure_sandbox(&client, &store, &spec(), "agentkit/coding@1", now_ms())
        .await
        .expect("create");

    // Parent writer admitted first.
    let pt = task("task-parent");
    let p_lease = client
        .acquire_writer(&ctx, &pt, 600_000)
        .await
        .expect("parent writer");

    // Child authority derived (read+write inside /workspace/src only).
    let parent0 = store
        .get(&support::tid("alice"), &sbx)
        .unwrap()
        .expect("parent binding");
    let ct = task("task-child");
    let parent = subagent::derive_child(
        &store,
        &parent0,
        &ct,
        Some("child-1".into()),
        &[
            SandboxCapability::SandboxRead,
            SandboxCapability::SandboxWrite,
        ],
        &["/workspace/src".to_string()],
    )
    .expect("derive child");

    // Child admitted as writer: supersedes the parent (worker epoch bump).
    let c_lease = client
        .acquire_writer(&ctx, &ct, 600_000)
        .await
        .expect("child writer");
    assert_eq!(c_lease.writer_epoch, p_lease.writer_epoch + 1);

    // Parent's mutating grant now fails deterministically at the worker.
    let g = write_grant(&client, SandboxCapability::SandboxWrite, &pt, &p_lease);
    let err = client
        .write(
            &g,
            &gp("/workspace/src/orphan.txt"),
            b"stale".to_vec(),
            None,
        )
        .await
        .expect_err("superseded parent writer must fail");
    assert_eq!(
        err.code,
        agentkit_sandbox_host::contracts::error::SandboxErrorCode::PolicyDenied
    );
    assert!(
        err.message.contains("stale write generation")
            || err
                .detail
                .as_deref()
                .is_some_and(|d| d.contains("stale write generation")),
        "{err:?}"
    );

    // The child (current writer) writes where its authority allows.
    let mut cg = scaffold_write_grant(
        client.binding(),
        SandboxCapability::SandboxWrite,
        &ct,
        &c_lease,
    );
    cg.permitted_paths = vec![gp("/workspace/src/new.rs")];
    subagent::authorize_child_grant(&parent, &ct, &cg).expect("child authorized");
    let receipt = client
        .write(
            &cg,
            &gp("/workspace/src/new.rs"),
            b"fn child() {}".to_vec(),
            None,
        )
        .await
        .expect("child write");
    assert!(receipt.created);

    // Child cannot reach outside its narrowed prefix (host gate).
    let mut esc = write_grant(&client, SandboxCapability::SandboxWrite, &ct, &c_lease);
    esc.permitted_paths = vec![gp("/workspace/outside.txt")];
    assert!(matches!(
        subagent::authorize_child_grant(&parent, &ct, &esc),
        Err(subagent::SubagentError::PathDenied(_))
    ));
}

#[tokio::test]
async fn reviewer_reads_snapshot_scope_but_cannot_write_or_exec() {
    let env = support::TestEnv::start().await;
    let store = InMemoryStore::new();
    let sbx = support::deterministic_sandbox("alice", "sess-r");
    let client = env.client("alice", "alice", "sess-r", sbx.as_str()).await;
    let ctx = manage_ctx("alice", "alice", "sess-r");
    ensure_sandbox(&client, &store, &spec(), "agentkit/coding@1", now_ms())
        .await
        .expect("create");

    // Seed content through the root writer.
    let rt = task("task-root");
    let rl = client
        .acquire_writer(&ctx, &rt, 600_000)
        .await
        .expect("root writer");
    let g = scaffold_write_grant(client.binding(), SandboxCapability::SandboxWrite, &rt, &rl);
    client
        .write(
            &g,
            &gp("/workspace/review.txt"),
            b"review me".to_vec(),
            None,
        )
        .await
        .expect("seed");

    let rt = task("task-reviewer");
    let parent = store
        .get(&support::tid("alice"), &sbx)
        .unwrap()
        .expect("binding");
    let parent = subagent::reviewer_snapshot(
        &store,
        &parent,
        &rt,
        Some("reviewer".into()),
        agentkit_sandbox_host::contracts::ids::ArtifactDigest::of_bytes(b"snapshot-1"),
        &["/workspace".to_string()],
    )
    .expect("reviewer derived");

    // Reviewer reads.
    let mut rg = scaffold_grant(client.binding(), SandboxCapability::SandboxRead);
    rg.task_id = Some(rt.clone());
    subagent::authorize_child_grant(&parent, &rt, &rg).expect("reviewer read authorized");
    let reply = client
        .read(&rg, &gp("/workspace/review.txt"), None, None)
        .await
        .expect("reviewer read");
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(&reply.data_b64)
            .expect("b64"),
        b"review me"
    );

    // Reviewer write/exec: refused by the host gate, never dispatched.
    let mut wg = scaffold_grant(client.binding(), SandboxCapability::SandboxWrite);
    wg.task_id = Some(rt.clone());
    assert!(matches!(
        subagent::authorize_child_grant(&parent, &rt, &wg),
        Err(subagent::SubagentError::ReviewerNotReadOnly(_))
    ));
    let mut xg = scaffold_grant(client.binding(), SandboxCapability::ProcessExec);
    xg.task_id = Some(rt.clone());
    assert!(matches!(
        subagent::authorize_child_grant(&parent, &rt, &xg),
        Err(subagent::SubagentError::ReviewerNotReadOnly(_))
    ));
}

#[tokio::test]
async fn parent_cancellation_stops_child_grants() {
    let env = support::TestEnv::start().await;
    let store = InMemoryStore::new();
    let sbx = support::deterministic_sandbox("alice", "sess-c");
    let client = env.client("alice", "alice", "sess-c", sbx.as_str()).await;
    let ctx = manage_ctx("alice", "alice", "sess-c");
    ensure_sandbox(&client, &store, &spec(), "agentkit/coding@1", now_ms())
        .await
        .expect("create");

    let ct = task("task-cancelled");
    let parent = store
        .get(&support::tid("alice"), &sbx)
        .unwrap()
        .expect("binding");
    let parent = subagent::derive_child(
        &store,
        &parent,
        &ct,
        Some("child-x".into()),
        &[SandboxCapability::SandboxRead],
        &["/workspace".to_string()],
    )
    .expect("derive");

    let mut g = scaffold_grant(client.binding(), SandboxCapability::SandboxRead);
    g.task_id = Some(ct.clone());
    subagent::authorize_child_grant(&parent, &ct, &g).expect("child acts pre-cancel");

    let cancelled = subagent::cancel_children(&store, &parent).expect("cancel");
    assert_eq!(cancelled, vec![ct.clone()]);

    let reloaded = store
        .get(&support::tid("alice"), &sbx)
        .unwrap()
        .expect("binding");
    assert!(matches!(
        subagent::authorize_child_grant(&reloaded, &ct, &g),
        Err(subagent::SubagentError::Cancelled)
    ));
    // Root authority keeps working (the parent still owns the sandbox).
    subagent::authorize_root_grant(&reloaded).expect("root acts");
}
