//! Single-writer lease gate (S09, T16): the worker is the authoritative
//! writer admission. Mutating grants (`SandboxWrite`, `ArtifactImport`)
//! must carry the live lease of the currently admitted task; stale
//! epochs, foreign holders, expired leases and missing leases all die at
//! the gate -- and the admission survives a worker crash via the journal.

mod support;

use agentkit_sandbox_contracts::api::messages::{
    GuestFsOp, HostToWorker, WorkerReplyBody, WorkerToHost,
};
use agentkit_sandbox_contracts::error::SandboxErrorCode;
use agentkit_sandbox_contracts::grant::WriterLeaseToken;
use agentkit_sandbox_contracts::ids::{RequestDigest, TaskId};
use agentkit_sandbox_contracts::policy::SandboxCapability;
use agentkit_sandbox_worker::guestlink::SimBehavior;
use agentkit_sandbox_worker::service::canonical_body;
use support::{Harness, gp, sid, tid};

fn write_op(path: &str, body: &[u8]) -> GuestFsOp {
    GuestFsOp::Write {
        path: gp(path),
        body_b64: support::base64_encode(body),
        expected_sha256: None,
        create_parents: true,
    }
}

async fn ready(h: &Harness) {
    h.create_ready("alice", "leasebox").await;
}

fn task(s: &str) -> TaskId {
    TaskId::new(s).unwrap()
}

/// Write succeeds only with the live lease of the admitted task.
#[tokio::test]
async fn write_requires_admitted_lease_and_succeeds_with_it() {
    let h = Harness::new();
    ready(&h).await;
    let op = write_op("/workspace/a.txt", b"hello");
    let d = RequestDigest::of_canonical_json(&canonical_body(&op).unwrap());
    let body_b64 = support::base64_encode(b"hello");

    // No lease: refused before any guest dispatch.
    let e = h
        .err(
            "w-no",
            10,
            &HostToWorker::FsWrite {
                grant: h.grant(
                    &tid("alice"),
                    &sid("leasebox"),
                    SandboxCapability::SandboxWrite,
                    d.clone(),
                    vec![gp("/workspace/a.txt")],
                    None,
                ),
                path: gp("/workspace/a.txt"),
                body_b64: body_b64.clone(),
                expected_sha256: None,
            },
        )
        .await;
    assert_eq!(e.code, SandboxErrorCode::PolicyDenied, "{e:?}");

    // Admitted writer: succeeds.
    let t = task("task-a");
    let lease = h
        .acquire_writer(&tid("alice"), &sid("leasebox"), &t, 600_000)
        .await;
    let _ = h
        .ok(
            "w-ok",
            11,
            &HostToWorker::FsWrite {
                grant: h.grant_w(
                    &tid("alice"),
                    &sid("leasebox"),
                    SandboxCapability::SandboxWrite,
                    d,
                    vec![gp("/workspace/a.txt")],
                    None,
                    Some(t.clone()),
                    Some(lease.clone()),
                ),
                path: gp("/workspace/a.txt"),
                body_b64,
                expected_sha256: None,
            },
        )
        .await;
}

/// Parent/child/sibling conflict: the newest admission is the ONLY writer;
/// the superseded task's grants fail deterministically with the stale
/// write generation, never a silent double write.
#[tokio::test]
async fn newer_admission_supersedes_previous_writer() {
    let h = Harness::new();
    ready(&h).await;
    let (a, b) = (task("task-a"), task("task-b"));
    let lease_a = h
        .acquire_writer(&tid("alice"), &sid("leasebox"), &a, 600_000)
        .await;
    let lease_b = h
        .acquire_writer(&tid("alice"), &sid("leasebox"), &b, 600_000)
        .await;
    assert_eq!(lease_b.writer_epoch, lease_a.writer_epoch + 1);

    let op = write_op("/workspace/x.txt", b"x");
    let d = RequestDigest::of_canonical_json(&canonical_body(&op).unwrap());
    let e = h
        .err(
            "w-stale",
            20,
            &HostToWorker::FsWrite {
                grant: h.grant_w(
                    &tid("alice"),
                    &sid("leasebox"),
                    SandboxCapability::SandboxWrite,
                    d.clone(),
                    vec![gp("/workspace/x.txt")],
                    None,
                    Some(a.clone()),
                    Some(lease_a.clone()),
                ),
                path: gp("/workspace/x.txt"),
                body_b64: support::base64_encode(b"x"),
                expected_sha256: None,
            },
        )
        .await;
    assert_eq!(e.code, SandboxErrorCode::PolicyDenied, "{e:?}");
    assert!(
        e.message.contains("stale write generation"),
        "must name the epoch conflict: {e:?}"
    );

    // The current writer still writes.
    let _ = h
        .ok(
            "w-cur",
            21,
            &HostToWorker::FsWrite {
                grant: h.grant_w(
                    &tid("alice"),
                    &sid("leasebox"),
                    SandboxCapability::SandboxWrite,
                    d,
                    vec![gp("/workspace/x.txt")],
                    None,
                    Some(b.clone()),
                    Some(lease_b),
                ),
                path: gp("/workspace/x.txt"),
                body_b64: support::base64_encode(b"x"),
                expected_sha256: None,
            },
        )
        .await;
}

/// Holder/generation tampering and expiry all refuse at the gate.
#[tokio::test]
async fn holder_mismatch_stale_generation_and_expiry_refused() {
    let h = Harness::new();
    ready(&h).await;
    let t = task("task-holder");
    let lease = h
        .acquire_writer(&tid("alice"), &sid("leasebox"), &t, 600_000)
        .await;

    let try_write =
        |id: &'static str, seq: u64, grant_task: Option<TaskId>, tok: Option<WriterLeaseToken>| {
            let h = &h;
            async move {
                let op = write_op("/workspace/t.txt", b"t");
                let d = RequestDigest::of_canonical_json(&canonical_body(&op).unwrap());
                h.err(
                    id,
                    seq,
                    &HostToWorker::FsWrite {
                        grant: h.grant_w(
                            &tid("alice"),
                            &sid("leasebox"),
                            SandboxCapability::SandboxWrite,
                            d,
                            vec![gp("/workspace/t.txt")],
                            None,
                            grant_task,
                            tok,
                        ),
                        path: gp("/workspace/t.txt"),
                        body_b64: support::base64_encode(b"t"),
                        expected_sha256: None,
                    },
                )
                .await
            }
        };

    // Grant acted by a different task than the lease holder.
    let e = try_write("w-fk", 30, Some(task("task-intruder")), Some(lease.clone())).await;
    assert_eq!(e.code, SandboxErrorCode::PolicyDenied, "{e:?}");
    assert!(e.message.contains("holder"), "{e:?}");

    // Grant without any task at all.
    let e = try_write("w-nt", 31, None, Some(lease.clone())).await;
    assert_eq!(e.code, SandboxErrorCode::PolicyDenied, "{e:?}");

    // Token tampered to a future generation.
    let mut fake_gen = lease.clone();
    fake_gen.instance_generation = support::genof(99);
    let e = try_write("w-gen", 32, Some(t.clone()), Some(fake_gen)).await;
    assert!(
        matches!(
            e.code,
            SandboxErrorCode::PolicyDenied | SandboxErrorCode::StaleGeneration
        ),
        "generation-tampered lease must be refused, got {e:?}"
    );

    // Expired lease.
    let te = task("task-exp");
    let expired = h
        .acquire_writer(&tid("alice"), &sid("leasebox"), &te, 1)
        .await;
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let e = try_write("w-exp", 33, Some(te.clone()), Some(expired)).await;
    assert_eq!(e.code, SandboxErrorCode::PolicyDenied, "{e:?}");
    let evidence = format!("{} {:?}", e.message, e.detail);
    assert!(evidence.contains("expired"), "{evidence}");
}

/// Crash safety: the admission is durable. After a worker restart the
/// admitted writer keeps writing and the superseded writer stays dead.
#[tokio::test]
async fn admission_survives_worker_restart() {
    let h = Harness::new();
    ready(&h).await;
    let (a, b) = (task("task-a"), task("task-b"));
    let lease_a = h
        .acquire_writer(&tid("alice"), &sid("leasebox"), &a, 600_000)
        .await;
    let lease_b = h
        .acquire_writer(&tid("alice"), &sid("leasebox"), &b, 600_000)
        .await;
    assert_eq!(lease_b.writer_epoch, lease_a.writer_epoch + 1);

    // Admission is durable: the journal holds both WriterAcquired records
    // in order, so replay cannot roll the epoch back after a crash.
    let kinds: Vec<String> = h
        .journal_records()
        .iter()
        .filter_map(|r| match r {
            agentkit_sandbox_worker::journal::JournalRecord::WriterAcquired {
                epoch,
                holder_task,
                ..
            } => Some(format!("{epoch}:{}", holder_task.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(
        kinds,
        vec![
            format!("{}:task-a", lease_a.writer_epoch),
            format!("{}:task-b", lease_b.writer_epoch)
        ],
        "durable admission records"
    );

    let svc = h.restart_with(std::sync::Arc::clone(&h.fake));

    // The restarted worker must refuse BOTH writers -- the current one
    // (nothing may run before reconciliation, S14) and above all the
    // superseded one (the crash may never resurrect a dead writer, S09).
    let op = write_op("/workspace/r.txt", b"r");
    let d = RequestDigest::of_canonical_json(&canonical_body(&op).unwrap());
    for (tag, grant_task, tok) in [
        ("current", b.clone(), lease_b.clone()),
        ("stale", a.clone(), lease_a.clone()),
    ] {
        let reply = support::call_svc(
            &svc,
            "w-post",
            41,
            &HostToWorker::FsWrite {
                grant: h.grant_w(
                    &tid("alice"),
                    &sid("leasebox"),
                    SandboxCapability::SandboxWrite,
                    d.clone(),
                    vec![gp("/workspace/r.txt")],
                    None,
                    Some(grant_task),
                    Some(tok),
                ),
                path: gp("/workspace/r.txt"),
                body_b64: support::base64_encode(b"r"),
                expected_sha256: None,
            },
        )
        .await;
        match reply {
            WorkerToHost::Reply {
                outcome: agentkit_sandbox_contracts::api::ReplyOutcomeShape::Err { error },
                ..
            } => {
                assert!(
                    matches!(
                        error.code,
                        SandboxErrorCode::PolicyDenied | SandboxErrorCode::Quarantined
                    ),
                    "post-restart {tag} writer must be refused, got {error:?}"
                );
            }
            other => panic!("post-restart {tag} writer was ADMITTED: {other:?}"),
        }
    }
}

/// Exec is not gated by the writer lease (policy exec grants keep working
/// without admission); the gate covers guest-filesystem mutations only.
#[tokio::test]
async fn exec_without_lease_unaffected() {
    let h = Harness::new();
    ready(&h).await;
    h.set_sim_behavior(Some(SimBehavior {
        stdout: b"ok".to_vec(),
        ..Default::default()
    }));
    let mut req = h.exec_request("ex-lease", "/bin/true");
    let d = support::digest_of(&req);
    req.request_digest = d.clone();
    let body = h
        .ok(
            "ex",
            50,
            &HostToWorker::ExecStart {
                grant: h.grant(
                    &tid("alice"),
                    &sid("leasebox"),
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
        "{body:?}"
    );
}
