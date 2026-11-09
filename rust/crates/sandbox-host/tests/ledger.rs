//! Fenced quota reservation ledger (§5.4, S10/S17, M3).
//!
//! The ledger is the host's authoritative capacity accounting: grants are
//! checked against live reservations inside one `BEGIN IMMEDIATE`
//! transaction, and every release must present the ticket issued by the
//! matching grant. A stale host decision (old launch, replayed destroy)
//! must never free capacity a newer generation owns.

mod support;

use agentkit_sandbox_host::sqlite_store::{LedgerError, SqliteStore};
use agentkit_sandbox_worker::registry::QuotaCeilings;

use support::{genof, sid, tid};

fn store() -> (tempfile::TempDir, SqliteStore) {
    let tmp = tempfile::tempdir().unwrap();
    let s = SqliteStore::open(&tmp.path().join("sandbox.db")).expect("open");
    (tmp, s)
}

fn ceilings(live: u32, vcpus: u32, mem: u32, disk: u64) -> QuotaCeilings {
    QuotaCeilings {
        max_live_vms_per_tenant: live,
        max_live_vms_global: 64,
        max_vcpus_per_tenant: vcpus,
        max_memory_mib_per_tenant: mem,
        max_disk_mib_per_tenant: disk,
    }
}

#[test]
fn live_vm_ceiling_enforced_and_freed_by_release() {
    let (_t, s) = store();
    let a = tid("alice");
    s.set_ceilings(&a, &ceilings(2, 16, 65_536, 1_048_576))
        .expect("ceilings");

    let t1 = s
        .reserve(&a, &sid("vm-1"), genof(1), 2, 512, 1024)
        .expect("r1");
    let t2 = s
        .reserve(&a, &sid("vm-2"), genof(1), 2, 512, 1024)
        .expect("r2");
    let err = s
        .reserve(&a, &sid("vm-3"), genof(1), 2, 512, 1024)
        .expect_err("third live VM must exceed ceiling");
    assert!(matches!(err, LedgerError::Exceeded(_)), "{err:?}");
    assert_eq!(s.usage(&a).expect("usage").live_vms, 2);

    // Releasing one slot admits the next launch.
    s.release(&t2).expect("release");
    let t3 = s
        .reserve(&a, &sid("vm-3"), genof(1), 2, 512, 1024)
        .expect("re-admitted");
    assert_eq!(s.usage(&a).expect("usage").live_vms, 2);

    s.release(&t1).expect("release 1");
    s.release(&t3).expect("release 3");
    assert_eq!(s.usage(&a).expect("usage"), Default::default());
}

#[test]
fn vcpu_memory_and_disk_ceilings_enforced() {
    let (_t, s) = store();
    let a = tid("alice");
    s.set_ceilings(&a, &ceilings(8, 4, 1024, 2048))
        .expect("ceilings");
    s.reserve(&a, &sid("vm-1"), genof(1), 2, 512, 1024)
        .expect("r1");
    s.reserve(&a, &sid("vm-2"), genof(1), 2, 512, 1024)
        .expect("r2");

    // vcpus exhausted (2+2+1 > 4).
    assert!(matches!(
        s.reserve(&a, &sid("vm-3"), genof(1), 1, 0, 0),
        Err(LedgerError::Exceeded(_))
    ));
    // memory exhausted.
    assert!(matches!(
        s.reserve(&a, &sid("vm-3"), genof(1), 0, 512, 0),
        Err(LedgerError::Exceeded(_))
    ));
    // disk exhausted.
    assert!(matches!(
        s.reserve(&a, &sid("vm-3"), genof(1), 0, 0, 1024),
        Err(LedgerError::Exceeded(_))
    ));
    // Failed grants must not have reserved anything.
    assert_eq!(s.usage(&a).expect("usage").live_vms, 2);
}

#[test]
fn ceilings_are_per_tenant() {
    let (_t, s) = store();
    let (a, b) = (tid("alice"), tid("bob"));
    s.set_ceilings(&a, &ceilings(1, 16, 65_536, 1_048_576))
        .expect("ceilings");
    s.reserve(&a, &sid("a-1"), genof(1), 1, 64, 64).expect("a1");
    // Alice is at her live-VM ceiling; Bob's own ledger is untouched.
    assert!(matches!(
        s.reserve(&a, &sid("a-2"), genof(1), 1, 64, 64),
        Err(LedgerError::Exceeded(_))
    ));
    s.reserve(&b, &sid("b-1"), genof(1), 1, 64, 64).expect("b1");
    assert_eq!(s.usage(&a).expect("u").live_vms, 1);
    assert_eq!(s.usage(&b).expect("u").live_vms, 1);
}

#[test]
fn stale_ticket_cannot_release_newer_generation() {
    let (_t, s) = store();
    let a = tid("alice");
    let old = s
        .reserve(&a, &sid("vm-1"), genof(1), 2, 512, 1024)
        .expect("g1");
    // Relaunch of the SAME sandbox (same generation, newer grant) advances
    // the row fence; the old ticket must die at the fence gate.
    let current = s
        .reserve(&a, &sid("vm-1"), genof(1), 2, 512, 1024)
        .expect("g2");
    assert_ne!(old.fence, current.fence);

    assert!(matches!(s.release(&old), Err(LedgerError::StaleFence)));
    // The refusal freed nothing: capacity is still held.
    assert_eq!(s.usage(&a).expect("usage").live_vms, 1);

    s.release(&current).expect("fenced release");
    assert_eq!(s.usage(&a).expect("usage"), Default::default());
}

#[test]
fn generation_mismatch_and_double_release_are_not_found() {
    let (_t, s) = store();
    let a = tid("alice");
    let t = s.reserve(&a, &sid("vm-1"), genof(1), 1, 64, 64).expect("g");

    let mut wrong_gen = t.clone();
    wrong_gen.instance_generation = genof(2);
    assert!(matches!(s.release(&wrong_gen), Err(LedgerError::NotFound)));

    s.release(&t).expect("release");
    // Released reservations cannot be released again (no double-free of
    // capacity budget).
    assert!(matches!(s.release(&t), Err(LedgerError::NotFound)));

    assert!(matches!(
        s.release(&{
            let mut ghost = t.clone();
            ghost.sandbox_id = sid("never-reserved");
            ghost
        }),
        Err(LedgerError::NotFound)
    ));
}

#[test]
fn force_release_reconciles_orphan_reservations() {
    let (_t, s) = store();
    let a = tid("alice");
    let t = s.reserve(&a, &sid("vm-1"), genof(1), 1, 64, 64).expect("g");
    // Ticket lost (host crash before persisting it): reconciliation drops
    // the row after the physical state is proven gone.
    s.force_release(&a, &sid("vm-1")).expect("force");
    assert_eq!(s.usage(&a).expect("usage"), Default::default());
    assert!(matches!(s.release(&t), Err(LedgerError::NotFound)));
    // Idempotent.
    s.force_release(&a, &sid("vm-1")).expect("force again");
}

#[test]
fn ledger_survives_reopen_with_tickets() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("sandbox.db");
    let a = tid("alice");
    let t = {
        let s = SqliteStore::open(&path).expect("open");
        let t = s
            .reserve(&a, &sid("vm-1"), genof(7), 2, 512, 1024)
            .expect("g");
        assert_eq!(s.usage(&a).expect("usage").memory_mib, 512);
        t
    };
    // "Crash" + reopen: accounting and the fence epoch must be intact.
    let s = SqliteStore::open(&path).expect("reopen");
    assert_eq!(s.usage(&a).expect("usage").live_vms, 1);
    let active = s.active_reservations().expect("active");
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].sandbox_id, sid("vm-1"));
    assert_eq!(active[0], t, "reconcile view must match the grant ticket");
    s.release(&t).expect("post-crash release");
    assert_eq!(s.usage(&a).expect("usage"), Default::default());
}

#[test]
fn schema_version_guard() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("sandbox.db");
    {
        let s = SqliteStore::open(&path).expect("create");
        s.reserve(&tid("alice"), &sid("vm-1"), genof(1), 1, 1, 1)
            .expect("g");
    }
    // Foreign NEWER schema: refuse rather than guess.
    {
        let c = rusqlite::Connection::open(&path).expect("raw");
        c.pragma_update(None, "user_version", 99).expect("bump");
    }
    assert!(
        SqliteStore::open(&path).is_err(),
        "newer schema must refuse"
    );

    // Legacy (version 0) file migrates in place and keeps rows: the
    // tables are CREATE IF NOT EXISTS, so data written by a version-0
    // schema survives the upgrade path.
    {
        let c = rusqlite::Connection::open(&path).expect("raw");
        c.pragma_update(None, "user_version", 0).expect("downgrade");
    }
    let s = SqliteStore::open(&path).expect("migrate open");
    assert_eq!(s.usage(&tid("alice")).expect("usage").live_vms, 1);
}
