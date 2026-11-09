//! Fault injection at effect/journal boundaries and crash-time truthfulness
//! (§7, §14.5, T19/T20 at L1): a crash may lose observations, never let the
//! worker claim an outcome it cannot prove.

mod support;

use std::sync::Arc;

use agentkit_sandbox_contracts::api::ExecEventKind;
use agentkit_sandbox_contracts::api::messages::{HostToWorker, WorkerReplyBody, WorkerToHost};
use agentkit_sandbox_contracts::error::SandboxErrorCode;
use agentkit_sandbox_contracts::policy::SandboxCapability;
use agentkit_sandbox_worker::backend::ReconcileDecision;
use agentkit_sandbox_worker::backend::fake::FaultPoint;
use agentkit_sandbox_worker::guestlink::SimBehavior;
use agentkit_sandbox_worker::service::WorkerService;

use support::{Harness, digest_of, eid, genof, gp, sid, tid};

async fn exec_and_wait_terminal(h: &Harness, sandbox: &str, exec: &str) -> bool {
    let mut req = h.exec_request(exec, "true");
    let d = digest_of(&req);
    req.request_digest = d.clone();
    let start_grant = h.grant(
        &tid("alice"),
        &sid(sandbox),
        SandboxCapability::ProcessExec,
        d,
        vec![gp("/workspace")],
        Some(gp("/workspace")),
    );
    match h
        .call(
            &format!("start-{exec}"),
            1,
            &HostToWorker::ExecStart {
                grant: start_grant,
                request: Box::new(req),
            },
        )
        .await
    {
        WorkerToHost::Reply {
            outcome:
                agentkit_sandbox_contracts::api::ReplyOutcomeShape::Ok {
                    body: WorkerReplyBody::ExecStarted { .. },
                },
            ..
        } => {}
        other => panic!("exec start failed: {other:?}"),
    }
    let events_grant = h.grant(
        &tid("alice"),
        &sid(sandbox),
        SandboxCapability::ProcessExec,
        digest_of(&(eid(exec), 0u64)),
        vec![gp("/workspace")],
        Some(gp("/workspace")),
    );
    // Poll briefly; the sim is fast when it sends anything at all.
    for i in 0..20 {
        match h
            .call(
                &format!("poll-{exec}-{i}"),
                50 + i,
                &HostToWorker::ExecEvents {
                    grant: events_grant.clone(),
                    execution_id: eid(exec),
                    after_sequence: 0,
                },
            )
            .await
        {
            WorkerToHost::Reply {
                outcome:
                    agentkit_sandbox_contracts::api::ReplyOutcomeShape::Ok {
                        body: WorkerReplyBody::Events { events },
                    },
                ..
            } => {
                if events
                    .iter()
                    .any(|e| matches!(e.kind, ExecEventKind::Exited { .. }))
                {
                    return true;
                }
            }
            _ => {}
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    false
}

#[tokio::test]
async fn crash_after_provision_surfaces_orphan_never_phantom_ready() {
    let h = Harness::new();
    let s = sid("fault-prov");
    h.faults.arm(&s, FaultPoint::PostProvision);
    let (env, launch) = h.launch(&tid("alice"), &s, genof(1));
    let err = h
        .err(
            "create-1",
            1,
            &HostToWorker::CreateVm {
                envelope: env,
                launch,
            },
        )
        .await;
    assert_eq!(err.code, SandboxErrorCode::Unavailable);

    // Physical truth: the provision effect survived the injected crash.
    assert!(matches!(
        h.fake.reconcile_decision(&s),
        ReconcileDecision::Quarantine
    ));

    // A worker restart over the SAME physical backend must not claim the
    // sandbox exists as a logical VM (no phantom inventory entry).
    let svc = h.restart_with(Arc::clone(&h.fake));
    let body = support::call_svc(&svc, "inv", 1, &HostToWorker::ReconcileQuery {}).await;
    match body {
        WorkerToHost::Reply {
            outcome:
                agentkit_sandbox_contracts::api::ReplyOutcomeShape::Ok {
                    body: WorkerReplyBody::Inventory { inventory },
                },
            ..
        } => {
            // WAL-intent model: the create intent may replay, but the
            // worker may never present the orphan as RUNNING (the launch
            // journal record does not exist).
            for e in &inventory.sandboxes {
                if e.sandbox_id == s {
                    assert_ne!(
                        e.physical_state,
                        agentkit_sandbox_contracts::api::VmPhysState::Running,
                        "provision-crash sandbox presented as running"
                    );
                }
            }
        }
        other => panic!("inventory failed: {other:?}"),
    }
}

#[tokio::test]
async fn crash_after_launch_is_adoptable_not_claimed_running() {
    let h = Harness::new();
    let s = sid("fault-launch");
    h.faults.arm(&s, FaultPoint::PostLaunch);
    let (env, launch) = h.launch(&tid("alice"), &s, genof(1));
    let err = h
        .err(
            "create-2",
            1,
            &HostToWorker::CreateVm {
                envelope: env,
                launch,
            },
        )
        .await;
    assert_eq!(err.code, SandboxErrorCode::Unavailable);
    assert!(matches!(
        h.fake.reconcile_decision(&s),
        ReconcileDecision::Adopt
    ));

    // Replay keeps only what was journaled; the logical state machine may
    // not present Running without its journal record.
    let svc = h.restart_with(Arc::clone(&h.fake));
    match support::call_svc(&svc, "inv2", 1, &HostToWorker::ReconcileQuery {}).await {
        WorkerToHost::Reply {
            outcome:
                agentkit_sandbox_contracts::api::ReplyOutcomeShape::Ok {
                    body: WorkerReplyBody::Inventory { inventory },
                },
            ..
        } => {
            for e in &inventory.sandboxes {
                if e.sandbox_id == s {
                    assert_ne!(
                        e.physical_state,
                        agentkit_sandbox_contracts::api::VmPhysState::Running,
                        "journal replay claimed Running without a launched record"
                    );
                }
            }
        }
        other => panic!("inventory failed: {other:?}"),
    }
}

#[tokio::test]
async fn torn_journal_tail_is_truncated_not_fatal() {
    let h = Harness::new();
    h.create_ready("alice", "fault-torn").await;

    // Corrupt the journal tail exactly like a power loss mid-append.
    let dir = h.tmp.path().join("storage/journal");
    let path = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|e| e == "jsonl"))
        .expect("journal file");
    let before = std::fs::read(&path).unwrap();
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"{\"seq\":999,\"record\":{\"tamper")
        .unwrap();

    // Replay on restart: torn tail dropped, verified prefix intact.
    let svc = h.restart_with(Arc::clone(&h.fake));
    match support::call_svc(&svc, "inv3", 1, &HostToWorker::ReconcileQuery {}).await {
        WorkerToHost::Reply {
            outcome:
                agentkit_sandbox_contracts::api::ReplyOutcomeShape::Ok {
                    body: WorkerReplyBody::Inventory { inventory },
                },
            ..
        } => {
            assert!(
                inventory
                    .sandboxes
                    .iter()
                    .any(|e| e.sandbox_id.as_str() == "fault-torn")
            );
        }
        other => panic!("inventory after torn tail failed: {other:?}"),
    }
    let after = std::fs::read(&path).unwrap();
    assert!(after.starts_with(&before));
    assert!(
        after.ends_with(b"\n"),
        "torn tail must be truncated to a boundary"
    );
    assert!(after.len() < before.len() + 27);
}

#[tokio::test]
async fn lost_terminal_event_stays_nonterminal_across_restart() {
    let h = Harness::new();
    // The guest link is built at create time: arm the crash BEFORE create.
    h.set_sim_behavior(Some(SimBehavior {
        drop_exit: true,
        ..Default::default()
    }));
    h.create_ready("alice", "fault-exit").await;

    // No terminal event ever arrives, and cancel is a no-op (the sim's
    // process already finished): the worker must keep it NON-TERMINAL.
    assert!(!exec_and_wait_terminal(&h, "fault-exit", "lost-exit").await);

    // And the restarted worker still refuses to call it exited.
    let svc: Arc<WorkerService> = h.restart_with(Arc::clone(&h.fake));
    let g = h.grant(
        &tid("alice"),
        &sid("fault-exit"),
        SandboxCapability::ProcessExec,
        digest_of(&(eid("lost-exit"), 0u64)),
        vec![gp("/workspace")],
        Some(gp("/workspace")),
    );
    match support::call_svc(
        &svc,
        "ev-after",
        1,
        &HostToWorker::ExecEvents {
            grant: g,
            execution_id: eid("lost-exit"),
            after_sequence: 0,
        },
    )
    .await
    {
        // Either the worker quarantines the sandbox for reconciliation
        // (refusing to answer), or it answers with the durable buffer --
        // which MUST NOT contain a fabricated terminal event.
        WorkerToHost::Reply {
            outcome: agentkit_sandbox_contracts::api::ReplyOutcomeShape::Err { error },
            ..
        } => {
            assert_eq!(
                error.code,
                agentkit_sandbox_contracts::error::SandboxErrorCode::Quarantined,
                "post-restart exec refusal must be the quarantine code"
            );
        }
        WorkerToHost::Reply {
            outcome:
                agentkit_sandbox_contracts::api::ReplyOutcomeShape::Ok {
                    body: WorkerReplyBody::Events { events },
                },
            ..
        } => {
            assert!(
                !events
                    .iter()
                    .any(|e| matches!(e.kind, ExecEventKind::Exited { .. })),
                "restart fabricated a terminal event: {events:?}"
            );
        }
        other => panic!("events after restart: {other:?}"),
    }
}

#[tokio::test]
async fn spawn_crash_converges_via_cancel() {
    let h = Harness::new();
    // Model the spawn crash: the sim never emits Accepted/Spawned and
    // swallows the exec's own result, but a cancel that proves the
    // process dead still reports its terminal observation.
    h.set_sim_behavior(Some(SimBehavior {
        drop_accepted: true,
        ..Default::default()
    }));
    h.create_ready("alice", "fault-spawn").await;

    // Spawn observation lost: nothing terminal arrives on its own.
    assert!(!exec_and_wait_terminal(&h, "fault-spawn", "hang-exec").await);

    // Cancel converges: the guest kills the still-live process and the
    // terminal signal arrives.
    let g = h.grant(
        &tid("alice"),
        &sid("fault-spawn"),
        SandboxCapability::ProcessExec,
        digest_of(&eid("hang-exec")),
        vec![gp("/workspace")],
        Some(gp("/workspace")),
    );
    h.ok(
        "cancel-1",
        70,
        &HostToWorker::ExecCancel {
            grant: g,
            execution_id: eid("hang-exec"),
        },
    )
    .await;
    let g2 = h.grant(
        &tid("alice"),
        &sid("fault-spawn"),
        SandboxCapability::ProcessExec,
        digest_of(&(eid("hang-exec"), 0u64)),
        vec![gp("/workspace")],
        Some(gp("/workspace")),
    );
    for _ in 0..30 {
        if let WorkerToHost::Reply {
            outcome:
                agentkit_sandbox_contracts::api::ReplyOutcomeShape::Ok {
                    body: WorkerReplyBody::Events { events },
                },
            ..
        } = support::call_svc(
            &h.svc,
            "ev-cancel",
            80,
            &HostToWorker::ExecEvents {
                grant: g2.clone(),
                execution_id: eid("hang-exec"),
                after_sequence: 0,
            },
        )
        .await
        {
            if events.iter().any(|e| {
                matches!(
                    e.kind,
                    ExecEventKind::Exited {
                        signal: Some(9),
                        ..
                    }
                )
            }) {
                return; // converged
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("cancel did not converge the spawn-crashed execution");
}

use std::io::Write;
