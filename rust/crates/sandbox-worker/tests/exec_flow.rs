//! Execution admission, event ordering, duplicate/conflict semantics,
//! and cursor replay.

mod support;

use agentkit_sandbox_contracts::api::ExecEventKind;
use agentkit_sandbox_contracts::api::messages::{HostToWorker, WorkerReplyBody, WorkerToHost};
use agentkit_sandbox_contracts::error::SandboxErrorCode;
use agentkit_sandbox_contracts::ids::RequestDigest;
use agentkit_sandbox_contracts::policy::SandboxCapability;
use agentkit_sandbox_worker::service::canonical_body;
use support::{Harness, digest_of, eid, genof, gp, sid, tid};

#[tokio::test]
async fn exec_reports_accepted_spawned_exited_in_order() {
    let h = Harness::new();
    h.create_ready("alice", "sbx").await;
    let (_id, mut rx) = h.svc.register_push();

    let mut req = h.exec_request("exec-1", "true");
    let d = digest_of(&req);
    req.request_digest = d.clone();
    let grant = h.grant(
        &tid("alice"),
        &sid("sbx"),
        SandboxCapability::ProcessExec,
        d,
        vec![gp("/workspace")],
        Some(gp("/workspace")),
    );
    let body = h
        .ok(
            "exec-1",
            1,
            &HostToWorker::ExecStart {
                grant,
                request: Box::new(req),
            },
        )
        .await;
    let handle = match body {
        WorkerReplyBody::ExecStarted { handle } => handle,
        other => panic!("unexpected: {other:?}"),
    };
    assert!(!handle.replayed);

    // Pushed events arrive asynchronously for the authenticated subscriber.
    let mut pushed = Vec::new();
    for _ in 0..150 {
        while let Ok(msg) = rx.try_recv() {
            if let WorkerToHost::PushExecEvent(e) = msg {
                pushed.push(e);
            }
        }
        if !pushed.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    // Guest events land asynchronously; poll the durable buffer.
    let got = support::wait_for(3000, || {
        let h2 = &h;
        async move {
            let d =
                RequestDigest::of_canonical_json(&canonical_body(&(eid("exec-1"), 0u64)).unwrap());
            let g = h2.grant(
                &tid("alice"),
                &sid("sbx"),
                SandboxCapability::ProcessExec,
                d,
                vec![gp("/workspace")],
                Some(gp("/workspace")),
            );
            match h2
                .call(
                    "events-poll",
                    50,
                    &HostToWorker::ExecEvents {
                        grant: g,
                        execution_id: eid("exec-1"),
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
                } => events
                    .iter()
                    .any(|e| matches!(e.kind, ExecEventKind::Exited { .. })),
                _ => false,
            }
        }
    })
    .await;
    assert!(got, "guest exec events must reach the durable buffer");

    // Cursor replay via ExecEvents must agree with the push stream.
    let after = RequestDigest::of_canonical_json(&canonical_body(&(eid("exec-1"), 0u64)).unwrap());
    let grant2 = h.grant(
        &tid("alice"),
        &sid("sbx"),
        SandboxCapability::ProcessExec,
        after,
        vec![gp("/workspace")],
        Some(gp("/workspace")),
    );
    let body = h
        .ok(
            "events-1",
            2,
            &HostToWorker::ExecEvents {
                grant: grant2,
                execution_id: eid("exec-1"),
                after_sequence: 0,
            },
        )
        .await;
    let events = match body {
        WorkerReplyBody::Events { events } => events,
        other => panic!("unexpected: {other:?}"),
    };
    let kinds: Vec<String> = events
        .iter()
        .map(|e| {
            serde_json::to_value(&e.kind)
                .unwrap()
                .get("event")
                .and_then(|k| k.as_str())
                .unwrap_or("?")
                .to_string()
        })
        .collect();
    let accepted = kinds.iter().position(|k| k == "accepted");
    let spawned = kinds.iter().position(|k| k == "spawned");
    let exited = kinds.iter().position(|k| k == "exited");
    assert!(
        accepted < spawned && spawned < exited,
        "ordering broken: {kinds:?}"
    );
    assert!(!pushed.is_empty(), "subscriber must see pushed exec events");

    // Cursor semantics: asking after the spawned sequence omits it.
    let spawn_seq = events
        .iter()
        .find(|e| matches!(e.kind, ExecEventKind::Spawned { .. }))
        .map(|e| e.sequence)
        .expect("spawned event present");
    let after =
        RequestDigest::of_canonical_json(&canonical_body(&(eid("exec-1"), spawn_seq)).unwrap());
    let grant3 = h.grant(
        &tid("alice"),
        &sid("sbx"),
        SandboxCapability::ProcessExec,
        after,
        vec![gp("/workspace")],
        Some(gp("/workspace")),
    );
    let body = h
        .ok(
            "events-2",
            3,
            &HostToWorker::ExecEvents {
                grant: grant3,
                execution_id: eid("exec-1"),
                after_sequence: spawn_seq,
            },
        )
        .await;
    match body {
        WorkerReplyBody::Events { events } => {
            assert!(
                events.iter().all(|e| e.sequence > spawn_seq),
                "cursor leaked older events: {events:?}"
            );
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[tokio::test]
async fn duplicate_execution_identical_digest_replays_conflict_errors() {
    let h = Harness::new();
    h.create_ready("alice", "sbd").await;

    let mut req = h.exec_request("exec-dup", "true");
    let d = digest_of(&req);
    req.request_digest = d.clone();
    let mk = |req: &agentkit_sandbox_contracts::api::ExecRequest| {
        h.grant(
            &tid("alice"),
            &sid("sbd"),
            SandboxCapability::ProcessExec,
            digest_of(req),
            vec![gp("/workspace")],
            Some(gp("/workspace")),
        )
    };
    let first = match h
        .ok(
            "dup-a",
            1,
            &HostToWorker::ExecStart {
                grant: mk(&req),
                request: Box::new(req.clone()),
            },
        )
        .await
    {
        WorkerReplyBody::ExecStarted { handle } => handle,
        other => panic!("unexpected: {other:?}"),
    };
    assert!(!first.replayed);

    // Identical replay.
    let again = match h
        .ok(
            "dup-b",
            2,
            &HostToWorker::ExecStart {
                grant: mk(&req),
                request: Box::new(req),
            },
        )
        .await
    {
        WorkerReplyBody::ExecStarted { handle } => handle,
        other => panic!("unexpected: {other:?}"),
    };
    assert!(again.replayed, "duplicate must be flagged replayed");
    assert_eq!(again.execution_id, first.execution_id);
    assert_eq!(again.state, first.state);

    // Same execution_id, different body = protocol conflict.
    let mut conflicting = h.exec_request("exec-dup", "false");
    let cd = digest_of(&conflicting);
    conflicting.request_digest = cd;
    let err = h
        .err(
            "dup-c",
            3,
            &HostToWorker::ExecStart {
                grant: mk(&conflicting),
                request: Box::new(conflicting),
            },
        )
        .await;
    assert_eq!(err.code, SandboxErrorCode::ProtocolIncompatible);
}

#[tokio::test]
async fn exec_requires_a_live_sandbox_and_valid_digest() {
    let h = Harness::new();
    h.create_ready("alice", "sbv").await;

    // Self-declared request_digest that does not match the body.
    let mut req = h.exec_request("exec-bad", "true");
    req.request_digest = RequestDigest::of_canonical_json(b"not-the-body");
    let err = h
        .err(
            "bad-1",
            1,
            &HostToWorker::ExecStart {
                grant: h.grant(
                    &tid("alice"),
                    &sid("sbv"),
                    SandboxCapability::ProcessExec,
                    RequestDigest::of_canonical_json(b"not-the-body"),
                    vec![gp("/workspace")],
                    Some(gp("/workspace")),
                ),
                request: Box::new(req),
            },
        )
        .await;
    assert_eq!(err.code, SandboxErrorCode::ProtocolIncompatible);

    // Unknown sandbox: indistinguishable from cross-tenant (no disclosure).
    let mut req = h.exec_request("exec-ghost", "true");
    let d = digest_of(&req);
    req.request_digest = d.clone();
    let err = h
        .err(
            "ghost-1",
            2,
            &HostToWorker::ExecStart {
                grant: h.grant(
                    &tid("alice"),
                    &sid("nope"),
                    SandboxCapability::ProcessExec,
                    d,
                    vec![gp("/workspace")],
                    Some(gp("/workspace")),
                ),
                request: Box::new(req),
            },
        )
        .await;
    assert_eq!(err.code, SandboxErrorCode::TenantForbidden);
}

#[tokio::test]
async fn cancel_reports_state_without_claiming_death() {
    let h = Harness::new();
    h.create_ready("alice", "sbc").await;
    let mut req = h.exec_request("exec-cancel", "sleep");
    let d = digest_of(&req);
    req.request_digest = d.clone();
    h.ok(
        "cancel-start",
        1,
        &HostToWorker::ExecStart {
            grant: h.grant(
                &tid("alice"),
                &sid("sbc"),
                SandboxCapability::ProcessExec,
                d,
                vec![gp("/workspace")],
                Some(gp("/workspace")),
            ),
            request: Box::new(req),
        },
    )
    .await;

    let cancel_digest =
        RequestDigest::of_canonical_json(&canonical_body(&eid("exec-cancel")).unwrap());
    let body = h
        .ok(
            "cancel-1",
            2,
            &HostToWorker::ExecCancel {
                grant: h.grant(
                    &tid("alice"),
                    &sid("sbc"),
                    SandboxCapability::ProcessExec,
                    cancel_digest,
                    vec![gp("/workspace")],
                    Some(gp("/workspace")),
                ),
                execution_id: eid("exec-cancel"),
            },
        )
        .await;
    match body {
        WorkerReplyBody::ExecCancelled { out } => {
            assert_eq!(out.execution_id, eid("exec-cancel"));
            // The sim already reported exit; the worker reply reflects the
            // recorded command state, never a guest-claimed "killed" lie.
            assert!(!out.uncertain, "known outcome must not be marked uncertain");
        }
        other => panic!("unexpected: {other:?}"),
    }
    let _ = (genof(1), genof(1));
}
