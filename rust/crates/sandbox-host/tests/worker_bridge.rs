//! L1 bridge tests against the real worker binary (fake backend, no VMs):
//! handshake, digest-pinned lifecycle, workspace roundtrips, exec streams,
//! concurrency, crash-after-dispatch uncertainty semantics, and tenant
//! cross-call refusal on both sides.

mod support;

use agentkit_sandbox_host::contracts::api::ExecArgv;
use agentkit_sandbox_host::contracts::api::{
    ExecRequest, ListOptions, SandboxManager, SandboxWorkspaceIo, SearchQuery,
};
use agentkit_sandbox_host::contracts::error::SandboxErrorCode;
use agentkit_sandbox_host::contracts::ids::{ExecutionId, RequestDigest, ToolCallId};
use agentkit_sandbox_host::contracts::policy::SandboxCapability;
use agentkit_sandbox_host::contracts::state::CommandState;
use base64::Engine as _;

use support::{TestEnv, gp, manage_ctx, scaffold_grant, spec};

fn exec_request(id: &str, command: &str) -> ExecRequest {
    ExecRequest {
        execution_id: ExecutionId::new(id).unwrap(),
        tool_call_id: ToolCallId::new(format!("tc-{id}")).unwrap(),
        request_digest: RequestDigest::of_canonical_json(b""),
        argv: ExecArgv::Shell {
            command: command.into(),
        },
        cwd: gp("/workspace"),
        environment: Default::default(),
        timeout_sec: 30,
        max_output_bytes: 65_536,
        max_processes: 8,
    }
}

#[tokio::test]
async fn hello_handshake_and_version_mismatch_rejected() {
    let env = TestEnv::start().await;
    let hello = env.worker.client().hello_info().expect("handshaked");
    assert_eq!(hello.worker_id.as_str(), "test-worker");
    assert_eq!(hello.revision, 1);
    assert_eq!(
        hello.install_fingerprint.as_str(),
        support::INSTALL_FINGERPRINT
    );

    // Version 2 only => worker fails closed (S16).
    let err = env
        .worker
        .client()
        .hello_with_versions(vec![2])
        .await
        .expect_err("mismatched protocol must be rejected");
    assert!(
        matches!(err, agentkit_sandbox_host::client::HostError::Handshake(_)),
        "expected handshake rejection, got {err:?}"
    );
    // Supported set still works afterwards.
    env.worker
        .client()
        .hello_with_versions(vec![1])
        .await
        .expect("supported version accepted");
}

#[tokio::test]
async fn create_start_fs_roundtrip_exec_stream() {
    let env = TestEnv::start().await;
    let sbx = support::deterministic_sandbox("alice", "sess-rt");
    let client = env.client("alice", "alice", "sess-rt", sbx.as_str()).await;
    let ctx = manage_ctx("alice", "alice", "sess-rt");

    let sref = client.create(&ctx, &spec()).await.expect("create");
    assert_eq!(sref.sandbox_id, sbx);
    assert_eq!(
        sref.state,
        agentkit_sandbox_host::contracts::state::SandboxState::Ready
    );

    // start = create replay (no StartVm op on the wire).
    let info = client.start(&ctx, &sbx).await.expect("start replay");
    assert_eq!(
        info.physical_state,
        agentkit_sandbox_host::contracts::api::VmPhysState::Running
    );

    // write -> read -> list roundtrip against the fake guest.
    let b = client.binding();
    let wtask = agentkit_sandbox_contracts::ids::TaskId::new("task-wb1").unwrap();
    let wlease = client
        .acquire_writer(&ctx, &wtask, 600_000)
        .await
        .expect("writer admission");
    let write_grant =
        support::scaffold_write_grant(b, SandboxCapability::SandboxWrite, &wtask, &wlease);
    let body = b"fn main() { println!(\"hi\"); }\n".to_vec();
    let receipt = client
        .write(&write_grant, &gp("/workspace/main.rs"), body.clone(), None)
        .await
        .expect("write");
    assert!(receipt.created);
    assert_eq!(receipt.bytes_written, body.len() as u64);
    assert_eq!(
        receipt.sha256,
        agentkit_sandbox_host::contracts::ids::ArtifactDigest::of_bytes(&body)
    );

    let read_grant = scaffold_grant(b, SandboxCapability::SandboxRead);
    let reply = client
        .read(&read_grant, &gp("/workspace/main.rs"), None, None)
        .await
        .expect("read");
    let got = base64::engine::general_purpose::STANDARD
        .decode(&reply.data_b64)
        .expect("b64");
    assert_eq!(got, body);
    assert!(!reply.truncated);

    let list = client
        .list(&read_grant, &gp("/workspace"), ListOptions::default())
        .await
        .expect("list");
    assert!(
        list.entries.iter().any(|e| e.name == "main.rs"),
        "list: {list:?}"
    );

    // exec stream to Exited.
    let exec_grant = scaffold_grant(b, SandboxCapability::ProcessExec);
    let handle = client
        .exec(&exec_grant, exec_request("exec-rt", "echo hello"))
        .await
        .expect("exec");
    assert!(!handle.replayed);
    assert_eq!(handle.state, CommandState::Acked);

    let mut events = Vec::new();
    for _ in 0..50 {
        events = client
            .exec_events(&exec_grant, &handle.execution_id, 0)
            .await
            .expect("events");
        if events.iter().any(|e| {
            matches!(
                e.kind,
                agentkit_sandbox_host::contracts::api::ExecEventKind::Exited { .. }
            )
        }) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        events.iter().any(|e| matches!(
            e.kind,
            agentkit_sandbox_host::contracts::api::ExecEventKind::Spawned { .. }
        )),
        "spawned missing: {events:?}"
    );
    let exited = events
        .iter()
        .find_map(|e| match &e.kind {
            agentkit_sandbox_host::contracts::api::ExecEventKind::Exited {
                exit_code,
                timed_out,
                ..
            } => Some((*exit_code, *timed_out)),
            _ => None,
        })
        .expect("exited event");
    assert_eq!(exited, (Some(0), false));
    // Pushed events were delivered on the socket too.
    let pushed = client.drain_pushed_events();
    assert!(
        pushed.iter().any(|e| e.execution_id == handle.execution_id),
        "push channel silent"
    );
}

#[tokio::test]
async fn concurrent_requests_multiplex() {
    let env = TestEnv::start().await;
    let sbx = support::deterministic_sandbox("alice", "sess-cc");
    let client = env.client("alice", "alice", "sess-cc", sbx.as_str()).await;
    let ctx = manage_ctx("alice", "alice", "sess-cc");
    client.create(&ctx, &spec()).await.expect("create");
    let b = client.binding();

    // One admitted writer (S09) owns all eight concurrent writes of this
    // task: the single-writer invariant is per admission, not per call.
    let wtask = agentkit_sandbox_contracts::ids::TaskId::new("task-cc").unwrap();
    let wlease = client
        .acquire_writer(&ctx, &wtask, 600_000)
        .await
        .expect("writer admission");

    let mut tasks = Vec::new();
    for i in 0..8 {
        let c = client.clone();
        let b = b.clone();
        let (wt, wl) = (wtask.clone(), wlease.clone());
        tasks.push(tokio::spawn(async move {
            let g = support::scaffold_write_grant(&b, SandboxCapability::SandboxWrite, &wt, &wl);
            c.write(
                &g,
                &gp(&format!("/workspace/f{i}.txt")),
                format!("body-{i}").into_bytes(),
                None,
            )
            .await
            .expect("concurrent write")
        }));
    }
    for t in tasks {
        assert!(t.await.expect("join").bytes_written > 0);
    }
    let g = scaffold_grant(b, SandboxCapability::SandboxRead);
    let list = client
        .list(&g, &gp("/workspace"), ListOptions::default())
        .await
        .expect("list");
    assert_eq!(list.entries.len(), 8);

    // Concurrent mixed kinds.
    let c1 = client.clone();
    let c2 = client.clone();
    let b1 = b.clone();
    let b2 = b.clone();
    let (a, r) = tokio::join!(
        {
            let bb = b1.clone();
            async move {
                let g = scaffold_grant(&bb, SandboxCapability::SandboxRead);
                c1.search(
                    &g,
                    SearchQuery {
                        root: gp("/workspace"),
                        pattern: "body-3".into(),
                        literal: true,
                        ignore_case: false,
                        glob: None,
                        context: 0,
                        max_matches: 10,
                        max_entries: 100,
                        timeout_ms: 5_000,
                    },
                )
                .await
            }
        },
        async move {
            let g = scaffold_grant(&b2, SandboxCapability::SandboxRead);
            c2.list(&g, &gp("/workspace"), ListOptions::default()).await
        }
    );
    assert!(
        a.expect("search")
            .matches
            .iter()
            .any(|m| m.text == "body-3")
    );
    assert_eq!(r.expect("list").entries.len(), 8);
}

#[tokio::test]
async fn socket_death_mid_read_retries_once_ok() {
    let env = TestEnv::start().await;
    let sbx = support::deterministic_sandbox("alice", "sess-rd");
    let client = env.client("alice", "alice", "sess-rd", sbx.as_str()).await;
    let ctx = manage_ctx("alice", "alice", "sess-rd");
    client.create(&ctx, &spec()).await.expect("create");
    let b = client.binding();
    let wtask = agentkit_sandbox_contracts::ids::TaskId::new("task-rd").unwrap();
    let wlease = client
        .acquire_writer(&ctx, &wtask, 600_000)
        .await
        .expect("writer admission");
    let wg = support::scaffold_write_grant(b, SandboxCapability::SandboxWrite, &wtask, &wlease);
    client
        .write(&wg, &gp("/workspace/x.txt"), b"payload".to_vec(), None)
        .await
        .expect("seed write");

    // Sever right after the read frame leaves the socket: the worker still
    // answers it (on its side), but this connection is gone => the client
    // must reconnect and retry the idempotent read exactly once.
    client.arm_socket_death_after_next_write();
    let rg = scaffold_grant(b, SandboxCapability::SandboxRead);
    let reply = client
        .read(&rg, &gp("/workspace/x.txt"), None, None)
        .await
        .expect("read survives one retry");
    let body = base64::engine::general_purpose::STANDARD
        .decode(&reply.data_b64)
        .expect("b64");
    assert_eq!(body, b"payload");
    assert_eq!(client.retry_count(), 1, "exactly one retry");
    assert!(client.took_reconnect());
}

#[tokio::test]
async fn socket_death_mid_write_is_uncertain_not_retried() {
    let env = TestEnv::start().await;
    let sbx = support::deterministic_sandbox("alice", "sess-wr");
    let client = env.client("alice", "alice", "sess-wr", sbx.as_str()).await;
    let ctx = manage_ctx("alice", "alice", "sess-wr");
    client.create(&ctx, &spec()).await.expect("create");
    let b = client.binding();

    // Mutation after dispatch is lost: uncertain, never replayed (S07).
    client.arm_socket_death_after_next_write();
    let wg = scaffold_grant(b, SandboxCapability::SandboxWrite);
    let err = client
        .write(&wg, &gp("/workspace/u.txt"), b"mutate".to_vec(), None)
        .await
        .expect_err("write must resolve uncertain");
    assert_eq!(err.code, SandboxErrorCode::ExecutionUncertain);
    assert!(err.is_uncertain());
    assert_eq!(client.retry_count(), 0, "mutations never retry");

    // exec start is a mutation too; the frame reached the worker exactly
    // once (journal shows a single admitted record; client never resent).
    client.arm_socket_death_after_next_write();
    let eg = scaffold_grant(b, SandboxCapability::ProcessExec);
    let err = client
        .exec(&eg, exec_request("exec-lost", "sleep 5"))
        .await
        .expect_err("exec must resolve uncertain");
    assert_eq!(err.code, SandboxErrorCode::ExecutionUncertain);
    assert_eq!(client.retry_count(), 0);

    // Let the worker settle its ingestion of the received frame, then read
    // the journal: the client dispatched once, so one admitted record.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(
        env.count_in_journal("\"exec_admitted\""),
        1,
        "worker journal must show a single apply, got:\n{}",
        env.journal_text()
    );
}

#[tokio::test]
async fn kill_worker_mid_write_uncertain_and_never_replayed() {
    let mut env = TestEnv::start().await;
    let sbx = support::deterministic_sandbox("alice", "sess-kill");
    let client = env
        .client("alice", "alice", "sess-kill", sbx.as_str())
        .await;
    let ctx = manage_ctx("alice", "alice", "sess-kill");
    client.create(&ctx, &spec()).await.expect("create");
    let b = client.binding();

    client.arm_socket_death_after_next_write();
    let eg = scaffold_grant(b, SandboxCapability::ProcessExec);
    let dispatch = {
        let c = client.clone();
        let g = eg.clone();
        tokio::spawn(async move { c.exec(&g, exec_request("exec-kill", "sleep 30")).await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    // Hard kill: pending exec resolves uncertain, nothing is replayed.
    env.worker.kill().expect("kill");
    let err = dispatch.await.expect("join").expect_err("uncertain");
    assert_eq!(err.code, SandboxErrorCode::ExecutionUncertain);
    assert_eq!(
        err.retry,
        agentkit_sandbox_host::contracts::error::RetryClass::UncertainReconcile
    );
    assert_eq!(client.retry_count(), 0);

    // Bring the worker back on the same storage: journal replay records the
    // single admission; the client never re-dispatched.
    env.restart_worker().await;
    assert_eq!(env.count_in_journal("\"exec_admitted\""), 1);
    // A fresh client must also see the worker-side consequence: the
    // sandbox comes back quarantined (occupied-unknown), never Ready.
    let client2 = env
        .client("alice", "alice", "sess-kill", sbx.as_str())
        .await;
    let eg2 = scaffold_grant(client2.binding(), SandboxCapability::ProcessExec);
    let err = client2
        .exec(&eg2, exec_request("exec-after", "true"))
        .await
        .expect_err("replayed sandbox stays quarantined");
    assert_eq!(err.code, SandboxErrorCode::Quarantined);
}

#[tokio::test]
async fn tenant_cross_call_refused_both_layers() {
    let env = TestEnv::start().await;
    // Tenant B owns its own sandbox on the same worker.
    let sb_b = support::deterministic_sandbox("bob", "sess-b");
    let client_b = env.client("bob", "bob", "sess-b", sb_b.as_str()).await;
    let ctx_b = manage_ctx("bob", "bob", "sess-b");
    client_b.create(&ctx_b, &spec()).await.expect("bob create");

    let sb_a = support::deterministic_sandbox("alice", "sess-a");
    let client_a = env.client("alice", "alice", "sess-a", sb_a.as_str()).await;

    // Layer 1 (client, S02): a foreign AuthContext is refused before the
    // wire is touched.
    let foreign = manage_ctx("bob", "bob", "sess-b");
    let err = client_a
        .inspect(&foreign, &sb_a)
        .await
        .expect_err("foreign ctx refused");
    assert_eq!(err.code, SandboxErrorCode::TenantForbidden);

    // Layer 2 (worker registry): tenant A bound to tenant B's sandbox id —
    // the worker's tenant-scoped lookup denies without existence leak.
    let client_a_on_b = env.client("alice", "alice", "sess-a", sb_b.as_str()).await;
    let ctx_a = manage_ctx("alice", "alice", "sess-a");
    let err = client_a_on_b
        .inspect(&ctx_a, &sb_b)
        .await
        .expect_err("worker must deny cross-tenant");
    assert_eq!(err.code, SandboxErrorCode::TenantForbidden);
    assert_eq!(err.message, "no such sandbox for this tenant");

    // Grants outside the binding are refused client-side too.
    let mut rogue = scaffold_grant(client_a.binding(), SandboxCapability::SandboxRead);
    rogue.sandbox_id = sb_b.clone();
    let err = client_a
        .read(&rogue, &gp("/workspace/x"), None, None)
        .await
        .expect_err("rogue grant refused");
    assert_eq!(err.code, SandboxErrorCode::PolicyDenied);
}

#[tokio::test]
async fn digest_confusion_and_unknown_sandbox_errors_surface() {
    let env = TestEnv::start().await;
    let sbx = support::deterministic_sandbox("alice", "sess-dg");
    let client = env.client("alice", "alice", "sess-dg", sbx.as_str()).await;
    let ctx = manage_ctx("alice", "alice", "sess-dg");
    client.create(&ctx, &spec()).await.expect("create");
    let b = client.binding();

    // A path outside the grant's permitted roots surfaces the worker's S12
    // denial verbatim — never a host-side listing (S06). `/tmp/probe` is a
    // legal guest path but outside the grant's `/workspace` roots, so the
    // only possible denial is the worker's permits_path check.
    let g = scaffold_grant(b, SandboxCapability::SandboxRead);
    let err = client
        .list(&g, &gp("/tmp/probe"), ListOptions::default())
        .await
        .expect_err("path outside permitted roots");
    assert_eq!(err.code, SandboxErrorCode::PolicyDenied);
    assert_eq!(err.detail.as_deref(), Some("permits_path"));

    // Executing against a *stopped* sandbox is refused by the worker's
    // state gate — surfaced, never silently executed on the host (S06).
    client.stop(&ctx, &sbx).await.expect("stop");
    let eg = scaffold_grant(b, SandboxCapability::ProcessExec);
    let err = client
        .exec(&eg, exec_request("exec-stopped", "true"))
        .await
        .expect_err("stopped sandbox refuses exec");
    assert!(matches!(
        err.code,
        SandboxErrorCode::Unavailable | SandboxErrorCode::Quarantined
    ));
}
