//! VM lifecycle happy path and honest-partial-truth stop.

mod support;

use agentkit_sandbox_contracts::api::messages::{HostToWorker, WorkerReplyBody};
use agentkit_sandbox_contracts::state::SandboxState;
use support::{Harness, genof, sid, storage_disk_path, tid};

#[tokio::test]
async fn create_ready_stop_destroy_emits_journal_and_evidence() {
    let h = Harness::new();
    h.create_ready("alice", "sb1").await;

    // Journal holds the full effect history.
    let records = h.journal_records();
    let kinds: Vec<String> = records.iter().map(support::kind_of).collect();
    assert!(kinds.iter().any(|k| k == "vm_created"), "kinds: {kinds:?}");
    assert!(kinds.iter().any(|k| k == "vm_launched"), "kinds: {kinds:?}");
    assert!(
        kinds.iter().any(|k| k == "disk_created"),
        "kinds: {kinds:?}"
    );
    assert!(
        kinds.iter().any(|k| k == "reservation_acquire"),
        "kinds: {kinds:?}"
    );

    // Disk materialized under the worker storage root only (S20).
    assert!(storage_disk_path(&h, "alice", "sb1").exists());

    // Inspect: worker-side observation shows running with a pid.
    let (env, _) = h.launch(&tid("alice"), &sid("sb1"), genof(1));
    let body = h
        .ok(
            "inspect-1",
            2,
            &HostToWorker::InspectVm {
                envelope: h.envelope(
                    &tid("alice"),
                    &sid("sb1"),
                    genof(1),
                    agentkit_sandbox_contracts::policy::SandboxCapability::SandboxManage,
                ),
            },
        )
        .await;
    match body {
        WorkerReplyBody::Inspected { observed, .. } => {
            assert_eq!(
                observed.state,
                agentkit_sandbox_contracts::api::VmPhysState::Running
            );
            assert!(observed.hypervisor_pid.is_some());
        }
        other => panic!("unexpected: {other:?}"),
    }
    let _ = env;

    // Stop: conclusive evidence.
    let body = h
        .ok(
            "stop-1",
            3,
            &HostToWorker::StopVm {
                envelope: h.envelope(
                    &tid("alice"),
                    &sid("sb1"),
                    genof(1),
                    agentkit_sandbox_contracts::policy::SandboxCapability::SandboxManage,
                ),
                force: false,
                reason: "test".into(),
            },
        )
        .await;
    match body {
        WorkerReplyBody::Stopped { evidence } => {
            assert!(evidence.conclusive, "fake stop is conclusive");
        }
        other => panic!("unexpected: {other:?}"),
    }

    // Destroy: deletion receipt, storage gone, reservation released.
    let body = h
        .ok(
            "destroy-1",
            4,
            &HostToWorker::DestroyVm {
                envelope: h.envelope(
                    &tid("alice"),
                    &sid("sb1"),
                    genof(1),
                    agentkit_sandbox_contracts::policy::SandboxCapability::SandboxManage,
                ),
                force: true,
            },
        )
        .await;
    match body {
        WorkerReplyBody::Destroyed { receipt } => {
            assert!(receipt.storage_deleted);
            assert!(receipt.reservations_released);
            assert!(receipt.termination.conclusive);
            assert!(receipt.retained.is_empty(), "clean destroy retains nothing");
        }
        other => panic!("unexpected: {other:?}"),
    }
    assert!(!storage_disk_path(&h, "alice", "sb1").exists());

    let records = h.journal_records();
    let kinds: Vec<String> = records.iter().map(support::kind_of).collect();
    assert!(kinds.iter().any(|k| k == "vm_stopped"), "kinds: {kinds:?}");
    assert!(
        kinds.iter().any(|k| k == "vm_destroyed"),
        "kinds: {kinds:?}"
    );
    assert!(
        kinds.iter().any(|k| k == "disk_deleted"),
        "kinds: {kinds:?}"
    );
    assert!(
        kinds.iter().any(|k| k == "reservation_release"),
        "kinds: {kinds:?}"
    );

    // Registry agrees.
    let st = h
        .svc
        .sandbox_state(&tid("alice"), &sid("sb1"))
        .unwrap_or(SandboxState::Destroyed);
    assert_eq!(st, SandboxState::Destroyed);
}

#[tokio::test]
async fn wrong_boot_nonce_refuses_association_and_quarantines() {
    let h = Harness::new();
    h.set_sim_behavior(Some(support::sim_guest::wrong_nonce_behavior()));
    let (env, launch) = h.launch(&tid("alice"), &sid("nonce"), genof(1));
    let err = h
        .err(
            "nonce-bad",
            1,
            &HostToWorker::CreateVm {
                envelope: env,
                launch,
            },
        )
        .await;
    // Channel association failure: never a silent Ready, retry only via
    // reconcile.
    assert_eq!(
        err.code,
        agentkit_sandbox_contracts::error::SandboxErrorCode::ExecutionUncertain
    );
    assert!(
        err.detail
            .as_deref()
            .unwrap_or_default()
            .contains("boot nonce"),
        "detail: {:?}",
        err.detail
    );
    let _ = WorkerReplyBody::Created {
        vm: agentkit_sandbox_contracts::api::VmHandle {
            sandbox_id: sid("unused"),
            instance_generation: genof(1),
            backend: "fake".into(),
            hypervisor_pid: None,
        },
        observed: agentkit_sandbox_contracts::api::VmObservedState {
            state: agentkit_sandbox_contracts::api::VmPhysState::Unknown,
            hypervisor_pid: None,
            process_identity: None,
            last_observed_ms: 0,
        },
    };
}

#[tokio::test]
async fn stop_with_inconclusive_evidence_quarantines_and_retains() {
    let h = Harness::new();
    h.create_ready("alice", "sb2").await;
    h.fake.make_terminate_permanently_inconclusive(&sid("sb2"));

    let body = h
        .ok(
            "stop-bad",
            1,
            &HostToWorker::StopVm {
                envelope: h.envelope(
                    &tid("alice"),
                    &sid("sb2"),
                    genof(1),
                    agentkit_sandbox_contracts::policy::SandboxCapability::SandboxManage,
                ),
                force: false,
                reason: "test".into(),
            },
        )
        .await;
    match body {
        WorkerReplyBody::Stopped { evidence } => {
            assert!(!evidence.conclusive, "fault made termination unverifiable");
        }
        other => panic!("unexpected: {other:?}"),
    }
    let st = h
        .svc
        .sandbox_state(&tid("alice"), &sid("sb2"))
        .expect("still tracked");
    assert_eq!(st, SandboxState::Quarantined);

    // Destroy after quarantine: partial truth — disk retained, reservations
    // not silently released, receipt says so.
    let body = h
        .ok(
            "destroy-bad",
            2,
            &HostToWorker::DestroyVm {
                envelope: h.envelope(
                    &tid("alice"),
                    &sid("sb2"),
                    genof(1),
                    agentkit_sandbox_contracts::policy::SandboxCapability::SandboxManage,
                ),
                force: true,
            },
        )
        .await;
    match body {
        WorkerReplyBody::Destroyed { receipt } => {
            assert!(
                !receipt.retained.is_empty(),
                "unverified VM must be retained, receipt: {receipt:?}"
            );
            assert!(
                !receipt.storage_deleted || !receipt.reservations_released,
                "quarantined destroy must not claim full completion: {receipt:?}"
            );
        }
        other => panic!("unexpected: {other:?}"),
    }
}

/// T22 restart recovery (§11.4, S14, S01): journal replay quarantines
/// launched VMs; NOTHING may run until the worker independently proves the
/// physical state. A surviving VM is reclaimed with termination evidence,
/// and the only lawful re-boot is exactly the NEXT instance_generation.
/// Recovery is automatic (triggered by host `Hello`) but never optimistic.
#[tokio::test(flavor = "multi_thread")]
async fn worker_restart_reconciles_then_reboots_new_generation() {
    use agentkit_sandbox_contracts::api::ReplyOutcomeShape;
    use agentkit_sandbox_contracts::api::messages::WorkerToHost;
    use agentkit_sandbox_contracts::error::SandboxErrorCode;
    use agentkit_sandbox_contracts::ids::TaskId;
    use agentkit_sandbox_contracts::policy::SandboxCapability;
    use std::sync::Arc;

    let h = Harness::new();
    h.create_ready("alice", "sb1").await;
    // Worker dies, VM process survives (same shared backend).
    let svc = h.restart_with(Arc::clone(&h.fake));

    // Pre-reconcile: the replayed sandbox refuses everything.
    match support::call_svc(
        &svc,
        "acq-early",
        1,
        &HostToWorker::WriterAcquire {
            envelope: h.envelope(
                &tid("alice"),
                &sid("sb1"),
                genof(1),
                SandboxCapability::SandboxManage,
            ),
            task_id: TaskId::new("t1").unwrap(),
            ttl_ms: 600_000,
        },
    )
    .await
    {
        WorkerToHost::Reply {
            outcome: ReplyOutcomeShape::Err { error },
            ..
        } => assert_eq!(
            error.code,
            SandboxErrorCode::Quarantined,
            "must refuse before reconcile: {error:?}"
        ),
        other => panic!("pre-reconcile must refuse: {other:?}"),
    }

    // Host reconnect handshake triggers the reconcile pass automatically.
    match support::call_svc(
        &svc,
        "hello",
        2,
        &HostToWorker::Hello {
            host_id: "test-host".into(),
            protocol_versions: vec![agentkit_sandbox_contracts::PROTOCOL_VERSION],
        },
    )
    .await
    {
        WorkerToHost::HelloAck { .. } => {}
        other => panic!("hello: {other:?}"),
    }
    let settled = support::wait_for(20_000, {
        let svc2 = Arc::clone(&svc);
        move || {
            let svc2 = Arc::clone(&svc2);
            async move {
                match support::call_svc(&svc2, "inv", 3, &HostToWorker::ReconcileQuery {}).await {
                    WorkerToHost::Reply {
                        outcome:
                            ReplyOutcomeShape::Ok {
                                body: WorkerReplyBody::Inventory { inventory },
                            },
                        ..
                    } => inventory.sandboxes.iter().any(|s| {
                        s.sandbox_id == sid("sb1")
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

    // Generation-skip re-boot must be refused.
    let (env3, launch3) = h.launch(&tid("alice"), &sid("sb1"), genof(3));
    match support::call_svc(
        &svc,
        "create-gen3",
        4,
        &HostToWorker::CreateVm {
            envelope: env3,
            launch: launch3,
        },
    )
    .await
    {
        WorkerToHost::Reply {
            outcome: ReplyOutcomeShape::Err { error },
            ..
        } => assert_eq!(
            error.code,
            SandboxErrorCode::StaleGeneration,
            "generation skip must be refused: {error:?}"
        ),
        other => panic!("generation skip must be refused: {other:?}"),
    }

    // The lawful re-boot: exactly the next generation.
    let (env2, launch2) = h.launch(&tid("alice"), &sid("sb1"), genof(2));
    match support::call_svc(
        &svc,
        "create-gen2",
        5,
        &HostToWorker::CreateVm {
            envelope: env2,
            launch: launch2,
        },
    )
    .await
    {
        WorkerToHost::Reply {
            outcome:
                ReplyOutcomeShape::Ok {
                    body: WorkerReplyBody::Created { vm, .. },
                },
            ..
        } => assert_eq!(vm.instance_generation, genof(2)),
        other => panic!("re-boot must be admitted: {other:?}"),
    }

    // Writer admission pins the live generation.
    match support::call_svc(
        &svc,
        "acq-gen2",
        6,
        &HostToWorker::WriterAcquire {
            envelope: h.envelope(
                &tid("alice"),
                &sid("sb1"),
                genof(2),
                SandboxCapability::SandboxManage,
            ),
            task_id: TaskId::new("t2").unwrap(),
            ttl_ms: 60_000,
        },
    )
    .await
    {
        WorkerToHost::Reply {
            outcome:
                ReplyOutcomeShape::Ok {
                    body: WorkerReplyBody::WriterLease { token },
                },
            ..
        } => {
            assert_eq!(token.instance_generation, genof(2));
            assert_eq!(token.holder_task_id.as_str(), "t2");
        }
        other => panic!("gen-2 writer acquire: {other:?}"),
    }
}
