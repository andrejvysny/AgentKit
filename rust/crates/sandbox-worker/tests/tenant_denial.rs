//! Cross-tenant isolation: no inspection, no control, no existence leak.

mod support;

use agentkit_sandbox_contracts::api::messages::HostToWorker;
use agentkit_sandbox_contracts::error::SandboxErrorCode;
use agentkit_sandbox_contracts::ids::RequestDigest;
use agentkit_sandbox_contracts::policy::SandboxCapability;
use agentkit_sandbox_worker::service::canonical_body;
use support::{Harness, digest_of, eid, genof, gp, sid, tid};

#[tokio::test]
async fn foreign_tenant_cannot_stop_destroy_or_exec() {
    let h = Harness::new();
    h.create_ready("alice", "svr").await;

    // Bob tries to stop Alice's sandbox.
    let bob_env = h.envelope(
        &tid("bob"),
        &sid("svr"),
        genof(1),
        SandboxCapability::SandboxManage,
    );
    let err = h
        .err(
            "bob-stop",
            1,
            &HostToWorker::StopVm {
                envelope: bob_env,
                force: false,
                reason: "nope".into(),
            },
        )
        .await;
    assert_eq!(err.code, SandboxErrorCode::TenantForbidden);

    // Bob tries to destroy it.
    let bob_env = h.envelope(
        &tid("bob"),
        &sid("svr"),
        genof(1),
        SandboxCapability::SandboxManage,
    );
    let err = h
        .err(
            "bob-destroy",
            2,
            &HostToWorker::DestroyVm {
                envelope: bob_env,
                force: true,
            },
        )
        .await;
    assert_eq!(err.code, SandboxErrorCode::TenantForbidden);

    // Bob tries to exec in it.
    let mut req = h.exec_request("bob-exec", "true");
    let d = digest_of(&req);
    req.request_digest = d.clone();
    let err = h
        .err(
            "bob-exec",
            3,
            &HostToWorker::ExecStart {
                grant: h.grant(
                    &tid("bob"),
                    &sid("svr"),
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

    // And to read its files.
    let body = h
        .err(
            "bob-read",
            4,
            &HostToWorker::FsRead {
                grant: h.grant(
                    &tid("bob"),
                    &sid("svr"),
                    SandboxCapability::SandboxRead,
                    RequestDigest::of_canonical_json(b"unused"),
                    vec![gp("/workspace/secret")],
                    None,
                ),
                path: gp("/workspace/secret"),
                from: None,
                len: None,
            },
        )
        .await;
    assert_eq!(body.code, SandboxErrorCode::TenantForbidden);
}

#[tokio::test]
async fn denial_is_indistinguishable_from_missing() {
    let h = Harness::new();
    h.create_ready("alice", "secretbox").await;

    let foreign = probe_stop(&h, &sid("secretbox"), "probe-real").await;
    let missing = probe_stop(&h, &sid("never-existed"), "probe-fake").await;
    assert_eq!(foreign.code, missing.code);
    assert_eq!(foreign.message, missing.message);

    // Exec events cursor probes equally opaque.
    let e1 = probe_events(&h, &sid("secretbox"), "ev-real").await;
    let e2 = probe_events(&h, &sid("nope-never"), "ev-fake").await;
    assert_eq!(e1.code, e2.code);
    assert_eq!(e1.message, e2.message);
}

async fn probe_stop(
    h: &Harness,
    sandbox: &agentkit_sandbox_contracts::ids::SandboxId,
    rid: &str,
) -> agentkit_sandbox_contracts::error::SandboxError {
    h.err(
        rid,
        9,
        &HostToWorker::StopVm {
            envelope: h.envelope(
                &tid("bob"),
                sandbox,
                genof(1),
                SandboxCapability::SandboxManage,
            ),
            force: false,
            reason: "probe".into(),
        },
    )
    .await
}

async fn probe_events(
    h: &Harness,
    sandbox: &agentkit_sandbox_contracts::ids::SandboxId,
    rid: &str,
) -> agentkit_sandbox_contracts::error::SandboxError {
    h.err(
        rid,
        10,
        &HostToWorker::ExecEvents {
            grant: h.grant(
                &tid("bob"),
                sandbox,
                SandboxCapability::ProcessExec,
                RequestDigest::of_canonical_json(&canonical_body(&(eid("e1"), 0u64)).unwrap()),
                vec![gp("/workspace")],
                None,
            ),
            execution_id: eid("e1"),
            after_sequence: 0,
        },
    )
    .await
}
