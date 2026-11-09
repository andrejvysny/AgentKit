//! Crash recovery: journal replay rebuilds a truthful registry (T11 L1).

mod support;

use agentkit_sandbox_contracts::api::ReplyOutcomeShape;
use agentkit_sandbox_contracts::api::messages::{HostToWorker, WorkerReplyBody, WorkerToHost};
use agentkit_sandbox_worker::journal::Journal;
use support::{Harness, kind_of, storage_disk_path};

#[tokio::test]
async fn replay_after_crash_rebuilds_inventory_truthfully() {
    let h = Harness::new();
    h.create_ready("alice", "crashbox").await;
    let records = h.journal_records();
    let kinds: Vec<String> = records.iter().map(kind_of).collect();
    assert!(kinds.iter().any(|k| k == "vm_created"));
    assert!(kinds.iter().any(|k| k == "vm_launched"));
    assert!(kinds.iter().any(|k| k == "disk_created"));
    assert!(kinds.iter().any(|k| k == "reservation_acquire"));

    // Simulated SIGKILL: drop the service entirely, rebuild from disk.
    let svc2 = h.restart();
    let st = svc2
        .sandbox_state(&support::tid("alice"), &support::sid("crashbox"))
        .expect("sandbox must survive replay");
    // Replay never trusts the pre-crash live state into Ready: physical
    // state is unknown until reconcile proves it (S14).
    assert!(
        matches!(
            st,
            agentkit_sandbox_contracts::state::SandboxState::Requested
                | agentkit_sandbox_contracts::state::SandboxState::Provisioning
                | agentkit_sandbox_contracts::state::SandboxState::Quarantined
        ),
        "replayed state must be conservative, got {st:?}"
    );

    // Physical inventory: the disk still exists on disk -> occupied.
    let reply = svc2
        .handle(&frame_of("inv-1", 1, &HostToWorker::ReconcileQuery {}))
        .await;
    match reply {
        WorkerToHost::Reply {
            outcome:
                ReplyOutcomeShape::Ok {
                    body: WorkerReplyBody::Inventory { inventory },
                },
            ..
        } => {
            assert_eq!(inventory.worker_id.as_str(), "test-worker");
            let sb = inventory
                .sandboxes
                .iter()
                .find(|s| s.sandbox_id.as_str() == "crashbox")
                .expect("inventory must list the sandbox");
            // Occupied-unknown, never "stopped": an unproven VM is a VM.
            assert_ne!(
                sb.physical_state,
                agentkit_sandbox_contracts::api::VmPhysState::Stopped,
                "unverified physical state must not read as stopped"
            );
            assert!(storage_disk_path(&h, "alice", "crashbox").exists());
            assert!(
                !inventory.reservations.is_empty(),
                "reservations must survive replay (quota accounting)"
            );
        }
        other => panic!("inventory failed: {other:?}"),
    }

    // Torn tail after the last fsynced effect: replay truncates, keeps truth.
    let path = h.svc.journal_path();
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    use std::io::Write;
    f.write_all(b"{\"seq\":999,\"TORN_CRASH_MARKER\"").unwrap();
    drop(f);
    let recs = Journal::replay(&path).unwrap();
    assert_eq!(recs.len(), h.journal_records().len(), "torn tail truncated");
    assert!(recs.len() >= 4, "replay lost effects: {recs:?}");
    let svc3 = h.restart();
    assert!(
        svc3.sandbox_state(&support::tid("alice"), &support::sid("crashbox"))
            .is_some()
    );
}

fn frame_of(
    request_id: &str,
    sequence: u64,
    msg: &HostToWorker,
) -> agentkit_sandbox_contracts::protocol::WireFrame {
    agentkit_sandbox_contracts::protocol::WireFrame {
        header: agentkit_sandbox_contracts::protocol::FrameHeader {
            protocol_version: agentkit_sandbox_contracts::PROTOCOL_VERSION,
            request_id: request_id.into(),
            sandbox_id: None,
            instance_generation: None,
            execution_id: None,
            operation: "test".into(),
            sequence,
            deadline_ms: None,
            payload_len: 0,
            payload_digest: None,
        },
        body: serde_json::to_value(msg).unwrap(),
    }
}
