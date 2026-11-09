//! Bounded soak (M7 / T22 harness): long mixed-op streams against the
//! faithful fake backend exercising the same service paths production uses
//! -- writes, reads, lists, execs with journal growth, then a crash-restart
//! and continued operation on the rebuilt service. Scale with
//! `AGENTKIT_SOAK_SCALE=<n>`.
//! This is an L1 harness: it proves bounded, stable, replay-consistent
//! service behaviour, NOT VM isolation (that is L2/L3, see real_vm.rs).

mod support;

use agentkit_sandbox_contracts::PROTOCOL_VERSION;
use agentkit_sandbox_contracts::api::ReplyOutcomeShape;
use agentkit_sandbox_contracts::api::messages::{
    GuestFsOp, HostToWorker, WorkerReplyBody, WorkerToHost,
};
use agentkit_sandbox_contracts::grant::WorkspaceGrant;
use agentkit_sandbox_contracts::grant::WriterLeaseToken;
use agentkit_sandbox_contracts::ids::{RequestDigest, TaskId};
use agentkit_sandbox_contracts::policy::SandboxCapability;
use agentkit_sandbox_worker::journal::Journal;
use agentkit_sandbox_worker::service::WorkerService;
use std::sync::Arc;
use support::{Harness, base64_encode, gp, sid, tid};

fn scale() -> u64 {
    std::env::var("AGENTKIT_SOAK_SCALE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1)
}

async fn ok_body(
    svc: &Arc<WorkerService>,
    id: &str,
    seq: u64,
    msg: &HostToWorker,
) -> WorkerReplyBody {
    match support::call_svc(svc, id, seq, msg).await {
        WorkerToHost::Reply {
            outcome: ReplyOutcomeShape::Ok { body },
            ..
        } => body,
        WorkerToHost::Reply {
            outcome: ReplyOutcomeShape::Err { error },
            ..
        } => panic!("soak call {id} failed: {error:?}"),
        other => panic!("soak call {id}: unexpected {other:?}"),
    }
}

fn write_op(path: &str, body: &[u8]) -> GuestFsOp {
    GuestFsOp::Write {
        path: gp(path),
        body_b64: base64_encode(body),
        expected_sha256: None,
        create_parents: true,
    }
}

async fn write_file(
    h: &Harness,
    svc: &Arc<WorkerService>,
    lease: &WriterLeaseToken,
    seq: u64,
    path: &str,
    body: &[u8],
    generation: agentkit_sandbox_contracts::ids::InstanceGeneration,
    holder: &str,
) {
    let op = write_op(path, body);
    let d = RequestDigest::of_canonical_json(
        &agentkit_sandbox_worker::service::canonical_body(&op).unwrap(),
    );
    let grant = h.grant_w(
        &tid("alice"),
        &sid("soakbox"),
        SandboxCapability::SandboxWrite,
        d,
        vec![gp(path)],
        None,
        Some(TaskId::new(holder).unwrap()),
        Some(lease.clone()),
    );
    let grant = WorkspaceGrant {
        instance_generation: generation,
        ..grant
    };
    let body = ok_body(
        svc,
        &format!("soak-w-{seq}"),
        seq,
        &HostToWorker::FsWrite {
            grant,
            path: gp(path),
            body_b64: base64_encode(body),
            expected_sha256: None,
        },
    )
    .await;
    assert!(
        matches!(body, WorkerReplyBody::Write { .. }),
        "soak write {seq}: {body:?}"
    );
}

async fn read_file(
    h: &Harness,
    svc: &Arc<WorkerService>,
    seq: u64,
    path: &str,
    generation: agentkit_sandbox_contracts::ids::InstanceGeneration,
) -> Vec<u8> {
    let op = GuestFsOp::Read {
        path: gp(path),
        from: None,
        len: None,
    };
    let d = RequestDigest::of_canonical_json(
        &agentkit_sandbox_worker::service::canonical_body(&op).unwrap(),
    );
    let grant = WorkspaceGrant {
        instance_generation: generation,
        ..h.grant(
            &tid("alice"),
            &sid("soakbox"),
            SandboxCapability::SandboxRead,
            d,
            vec![gp(path)],
            None,
        )
    };
    match ok_body(
        svc,
        &format!("soak-r-{seq}"),
        seq,
        &HostToWorker::FsRead {
            grant,
            path: gp(path),
            from: None,
            len: None,
        },
    )
    .await
    {
        WorkerReplyBody::Read { reply } => {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD
                .decode(&reply.data_b64)
                .expect("valid b64")
        }
        other => panic!("soak read {seq}: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn sustained_mixed_ops_survive_restart_with_durable_state() {
    let n = 300 * scale();
    let started = std::time::Instant::now();
    let h = Harness::new();
    h.create_ready("alice", "soakbox").await;
    let svc = Arc::clone(&h.svc);
    let lease: WriterLeaseToken = h
        .acquire_writer(
            &tid("alice"),
            &sid("soakbox"),
            &TaskId::new("soak-writer").unwrap(),
            3_600_000,
        )
        .await;

    // --- Phase A: sustained mixed stream ---------------------------------
    let mut seq = 1000u64;
    let mut execs = 0u64;
    for i in 0..n {
        let path = format!("/workspace/soak/f{}.dat", i % 50);
        let payload = format!("iteration {i} tenant alice").into_bytes();
        write_file(
            &h,
            &svc,
            &lease,
            seq,
            &path,
            &payload,
            support::genof(1),
            "soak-writer",
        )
        .await;
        seq += 1;
        let got = read_file(&h, &svc, seq, &path, support::genof(1)).await;
        assert_eq!(got, payload, "soak read-back mismatch at {i}");
        seq += 1;
        if i % 10 == 0 {
            let lop = GuestFsOp::List {
                path: gp("/workspace/soak"),
                options: Default::default(),
            };
            let ld = RequestDigest::of_canonical_json(
                &agentkit_sandbox_worker::service::canonical_body(&lop).unwrap(),
            );
            let body = ok_body(
                &svc,
                &format!("soak-l-{seq}"),
                seq,
                &HostToWorker::FsList {
                    grant: h.grant(
                        &tid("alice"),
                        &sid("soakbox"),
                        SandboxCapability::SandboxRead,
                        ld,
                        vec![gp("/workspace/soak")],
                        None,
                    ),
                    path: gp("/workspace/soak"),
                    options: Default::default(),
                },
            )
            .await;
            assert!(
                matches!(body, WorkerReplyBody::List { .. }),
                "soak list: {body:?}"
            );
            seq += 1;
        }
        if i % 25 == 0 {
            let mut req = h.exec_request(&format!("soak-x{execs}"), "true");
            let d = support::digest_of(&req);
            req.request_digest = d.clone();
            let body = ok_body(
                &svc,
                &format!("soak-x-{execs}"),
                seq,
                &HostToWorker::ExecStart {
                    grant: h.grant(
                        &tid("alice"),
                        &sid("soakbox"),
                        SandboxCapability::ProcessExec,
                        d,
                        vec![gp("/workspace")],
                        Some(gp("/workspace")),
                    ),
                    request: Box::new(req),
                },
            )
            .await;
            assert!(
                matches!(body, WorkerReplyBody::ExecStarted { .. }),
                "soak exec: {body:?}"
            );
            execs += 1;
            seq += 1;
        }
    }

    // All exec admissions must reach a durable terminal report.
    let exited = support::wait_for(30_000, {
        let svc = Arc::clone(&svc);
        move || {
            let path = svc.journal_path();
            let need = execs;
            async move {
                Journal::replay(&path)
                    .map(|recs| {
                        recs.iter()
                            .filter(|r| support::kind_of(r) == "exec_exited_reported")
                            .count() as u64
                            >= need
                    })
                    .unwrap_or(false)
            }
        }
    })
    .await;
    assert!(exited, "exec terminals did not all land in the journal");

    // --- Phase B: journal invariants + replay determinism ----------------
    // The journal is durable state, not an op log: assert admission/exit
    // parity and monotonic growth, not one record per fs op.
    let journal_path = svc.journal_path();
    let recs_a = Journal::replay(&journal_path).expect("replay");
    let admitted = recs_a
        .iter()
        .filter(|r| support::kind_of(r) == "exec_admitted")
        .count() as u64;
    let exited_n = recs_a
        .iter()
        .filter(|r| support::kind_of(r) == "exec_exited_reported")
        .count() as u64;
    assert_eq!(admitted, execs, "exec admissions lost");
    assert_eq!(admitted, exited_n, "admitted != exited");
    assert!(
        recs_a.len() >= 60,
        "journal suspiciously small: {}",
        recs_a.len()
    );
    let recs_a2 = Journal::replay(&journal_path).expect("replay twice");
    assert_eq!(recs_a, recs_a2, "journal replay not deterministic");

    // --- Phase C: crash-restart, Hello-triggered reconcile, reboot -------
    let svc2 = h.restart_with(Arc::clone(&h.fake));
    // The VM process survived (same shared fake backend); the worker died.
    // Host reconnect begins with Hello, which auto-triggers the reconcile
    // pass (§11.4). Nothing may run while state is unproven:
    let early = support::call_svc(
        &svc2,
        "soak-early-acquire",
        899_000,
        &HostToWorker::WriterAcquire {
            envelope: h.envelope(
                &tid("alice"),
                &sid("soakbox"),
                support::genof(1),
                SandboxCapability::SandboxManage,
            ),
            task_id: TaskId::new("soak-early").unwrap(),
            ttl_ms: 600_000,
        },
    )
    .await;
    match early {
        WorkerToHost::Reply {
            outcome: ReplyOutcomeShape::Err { error },
            ..
        } => assert_eq!(
            error.code,
            agentkit_sandbox_contracts::error::SandboxErrorCode::Quarantined,
            "replayed sandbox must refuse before reconcile: {error:?}"
        ),
        other => panic!("pre-reconcile acquire must refuse: {other:?}"),
    }
    match support::call_svc(
        &svc2,
        "soak-hello",
        1,
        &HostToWorker::Hello {
            host_id: "soak-host".into(),
            protocol_versions: vec![PROTOCOL_VERSION],
        },
    )
    .await
    {
        WorkerToHost::HelloAck { .. } => {}
        other => panic!("hello: {other:?}"),
    }
    // Reconcile proves the survivor is dead-in-authority and terminates it:
    // wait for the physical record to show Stopped with no pid.
    let settled = support::wait_for(30_000, {
        let svc = Arc::clone(&svc2);
        move || {
            let svc = Arc::clone(&svc);
            async move {
                match support::call_svc(
                    &svc,
                    "soak-reconcile-poll",
                    2,
                    &HostToWorker::ReconcileQuery {},
                )
                .await
                {
                    WorkerToHost::Reply {
                        outcome:
                            ReplyOutcomeShape::Ok {
                                body: WorkerReplyBody::Inventory { inventory },
                            },
                        ..
                    } => inventory.sandboxes.iter().any(|s| {
                        s.sandbox_id == sid("soakbox")
                            && s.physical_state
                                == agentkit_sandbox_contracts::api::VmPhysState::Stopped
                    }),
                    _ => false,
                }
            }
        }
    })
    .await;
    assert!(
        settled,
        "reconcile did not terminate and clear the survivor"
    );

    // Reboot: same sandbox identity, exactly the next generation.
    let (env2, launch2) = h.launch(&tid("alice"), &sid("soakbox"), support::genof(2));
    let created2 = ok_body(
        &svc2,
        "soak-create-gen2",
        900_100,
        &HostToWorker::CreateVm {
            envelope: env2,
            launch: launch2,
        },
    )
    .await;
    assert!(
        matches!(created2, WorkerReplyBody::Created { .. }),
        "gen-2 reboot must succeed: {created2:?}"
    );

    // Durable admission moved to the rebuilt service: the crashed worker's
    // lease is dead (old generation), the new generation admits fresh.
    let lease2 = match ok_body(
        &svc2,
        "soak-acquire-post",
        940_000,
        &HostToWorker::WriterAcquire {
            envelope: h.envelope(
                &tid("alice"),
                &sid("soakbox"),
                support::genof(2),
                SandboxCapability::SandboxManage,
            ),
            task_id: TaskId::new("soak-post").unwrap(),
            ttl_ms: 600_000,
        },
    )
    .await
    {
        WorkerReplyBody::WriterLease { token } => token,
        other => panic!("post-reboot writer acquire: {other:?}"),
    };
    assert_eq!(
        lease2.instance_generation,
        support::genof(2),
        "lease must pin the live generation"
    );
    for k in 0..20u64 {
        let path = format!("/workspace/soak/post{k}.dat");
        write_file(
            &h,
            &svc2,
            &lease2,
            950_000 + k,
            &path,
            b"post restart",
            support::genof(2),
            "soak-post",
        )
        .await;
        let got = read_file(&h, &svc2, 960_000 + k, &path, support::genof(2)).await;
        assert_eq!(got, b"post restart".to_vec(), "post-restart read-back {k}");
    }

    // Journal never shrank across restart.
    let recs_b = Journal::replay(&svc2.journal_path()).expect("replay after restart");
    assert!(
        recs_b.len() >= recs_a.len(),
        "journal truncated across restart"
    );

    // Deterministic ceiling keeps the harness honest about pathologies.
    assert!(
        started.elapsed() < std::time::Duration::from_secs(240),
        "soak took {:?}",
        started.elapsed()
    );
}
