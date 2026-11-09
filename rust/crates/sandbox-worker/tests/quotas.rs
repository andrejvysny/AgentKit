//! Reservation accounting: transactional acquire/release, ceilings,
//! stale-release refusal.

mod support;

use agentkit_sandbox_contracts::error::SandboxErrorCode;
use agentkit_sandbox_worker::registry::{QuotaCeilings, Registry};
use support::{Harness, tid};

#[tokio::test]
async fn per_tenant_ceiling_is_enforced_and_released() {
    let ceilings = QuotaCeilings {
        max_live_vms_per_tenant: 1,
        ..QuotaCeilings::default()
    };
    let h = Harness::with_ceilings(ceilings);
    h.create_ready("alice", "q1").await;

    // Second live VM for alice: refused BEFORE any effect.
    let (env, launch) = h.launch(&tid("alice"), &support::sid("q2"), support::genof(1));
    let err = h
        .err(
            "q2",
            1,
            &agentkit_sandbox_contracts::api::messages::HostToWorker::CreateVm {
                envelope: env,
                launch,
            },
        )
        .await;
    assert_eq!(err.code, SandboxErrorCode::ResourceExhausted);
    // No partial effects for the refused VM.
    assert!(!support::storage_disk_path(&h, "alice", "q2").exists());

    // Release via destroy frees the slot.
    h.ok(
        "q1-destroy",
        2,
        &agentkit_sandbox_contracts::api::messages::HostToWorker::DestroyVm {
            envelope: h.envelope(
                &tid("alice"),
                &support::sid("q1"),
                support::genof(1),
                agentkit_sandbox_contracts::policy::SandboxCapability::SandboxManage,
            ),
            force: true,
        },
    )
    .await;
    let (env, launch) = h.launch(&tid("alice"), &support::sid("q2"), support::genof(1));
    h.ok(
        "q2b",
        3,
        &agentkit_sandbox_contracts::api::messages::HostToWorker::CreateVm {
            envelope: env,
            launch,
        },
    )
    .await;
}

#[tokio::test]
async fn global_ceiling_spans_tenants() {
    let ceilings = QuotaCeilings {
        max_live_vms_global: 1,
        ..QuotaCeilings::default()
    };
    let h = Harness::with_ceilings(ceilings);
    h.create_ready("alice", "g1").await;
    let (env, launch) = h.launch(&tid("bob"), &support::sid("g2"), support::genof(1));
    let err = h
        .err(
            "g2",
            1,
            &agentkit_sandbox_contracts::api::messages::HostToWorker::CreateVm {
                envelope: env,
                launch,
            },
        )
        .await;
    assert_eq!(err.code, SandboxErrorCode::ResourceExhausted);
}

#[test]
fn registry_release_is_transactional_and_refuses_stale() {
    let mut reg = Registry::default();
    let alice = tid("alice");
    let bob = tid("bob");
    reg.reserve(&alice, 2, 512, 1024).unwrap();
    // Over-committing beyond the default ceiling must fail atomically.
    let over = reg.reserve(&alice, u32::MAX, 1, 1);
    assert!(over.is_err(), "vcpu ceiling must bite");
    // A failed reserve must not have changed totals: same release succeeds.
    reg.release(&alice, 2, 512, 1024).unwrap();
    // Releasing what was never held: refused (stale / forged release).
    assert!(
        reg.release(&bob, 2, 512, 1024).is_err(),
        "unknown-tenant release must be refused"
    );
    assert!(
        reg.release(&alice, 2, 512, 1024).is_err(),
        "double release must be refused"
    );
}
