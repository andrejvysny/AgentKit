//! Recovery durability: an unresolved execution must never end up with
//! NEITHER its durable intent NOR a durable quarantine record.
//!
//! Reviewer reproductions covered here:
//!   (A) in-process `new`/reconcile quarantined in MEMORY and deleted the
//!       durable intent — dropping the coordinator discarded the only
//!       evidence and a fresh coordinator admitted modifying work. The fix:
//!       a blocking intent is retired only on positive group-exit evidence
//!       or explicit recovery; a memory-only quarantine never supersedes
//!       it. `second_admitted` must stay false across drop + recreate.
//!   (B) lock-dir reconcile deleted the intent BEFORE reading/publishing
//!       its replacement quarantine — an injected quarantine-read IO error
//!       left `intent_count_after_error = 0` and admitted the next writer.
//!       The fix: establish replacement evidence, persist it successfully,
//!       ONLY then retire the original; every failure path preserves it.
//!
//! No live subprocess is needed anywhere: an unknown/corrupt ownership
//! record (absurd owner pid) already requires blocking until cleanup or
//! explicit recovery is established (fail closed).

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::sync::LazyLock;
use std::time::Duration;

use agentkit_toolpacks_coding::coordination::{
    AcquireFileInput, AcquireWorkspaceInput, CommandCleanupIdentity, ExecutionIntent, LeaseHolder,
    QuarantineEvidence, WorkspaceCoordinator, WorkspaceLease, canonical_root_key,
    create_in_process_workspace_coordinator,
};
use agentkit_toolpacks_coding::coordination_lockdir::{
    LockDirCoordinatorOptions, create_lockdir_workspace_coordinator,
};
use agentkit_toolpacks_coding::errors::CodingError;
use agentkit_toolpacks_coding::fs_util::sha256;

static GATE: LazyLock<tokio::sync::Mutex<()>> = LazyLock::new(|| tokio::sync::Mutex::new(()));

fn gate() -> &'static tokio::sync::Mutex<()> {
    &GATE
}

fn holder(run: &str) -> LeaseHolder {
    LeaseHolder {
        chat_id: Some("chat".into()),
        run_id: run.into(),
        tool_call_id: Some("call-1".into()),
        tool: "run_command".into(),
        label: "make install".into(),
    }
}

/// The unknown/corrupt ownership record: an absurd owner pid can never be
/// signal-probed, so it classifies as an UNKNOWN survivor set — blocking
/// evidence until cleanup or explicit recovery, with no live process needed.
fn corrupt_intent(root: &str, execution_id: &str) -> ExecutionIntent {
    ExecutionIntent {
        root: root.to_owned(),
        identity: CommandCleanupIdentity {
            execution_id: execution_id.to_owned(),
            owner_pid: -1,
            pgid: None,
            spawned_at_ms: 0,
        },
        holder: holder("crashed-run"),
        spawned: false,
    }
}

async fn acquire(
    coordinator: &dyn WorkspaceCoordinator,
    root: &str,
    run: &str,
    max_wait_ms: u64,
) -> Result<WorkspaceLease, CodingError> {
    tokio::time::timeout(
        Duration::from_secs(30),
        coordinator.acquire_workspace(AcquireWorkspaceInput {
            root: root.to_owned(),
            holder: holder(run),
            cancellation: None,
            max_wait_ms: Some(max_wait_ms),
            execution_id: None,
        }),
    )
    .await
    .expect("the acquisition resolves promptly")
}

async fn acquire_edit(
    coordinator: &dyn WorkspaceCoordinator,
    root: &str,
    run: &str,
) -> Result<WorkspaceLease, CodingError> {
    tokio::time::timeout(
        Duration::from_secs(30),
        coordinator.acquire_file(AcquireFileInput {
            root: root.to_owned(),
            path: format!("{root}/f.txt"),
            holder: holder(run),
            cancellation: None,
            max_wait_ms: Some(200),
            execution_id: None,
        }),
    )
    .await
    .expect("the edit acquisition resolves promptly")
}

fn walk_files(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    files.push(path);
                }
            }
        }
    }
    files
}

/// Durable intent files in the in-process journal naming any of these
/// executions (the journal survives coordinator drops by construction;
/// this is the evidence a reopen reconciles from).
fn journal_intents(execution_ids: &[impl AsRef<str>]) -> Vec<String> {
    let journal = std::env::temp_dir().join("agentkit-exec-intents");
    walk_files(&journal)
        .into_iter()
        .filter_map(|path| path.file_name().map(|n| n.to_string_lossy().into_owned()))
        .filter(|name| execution_ids.iter().any(|id| name.contains(id.as_ref())))
        .collect()
}

/// Every durable file (intent records, quarantine record, locks) the lock
/// directory holds for any root.
fn lockdir_files(dir: &str) -> Vec<String> {
    walk_files(std::path::Path::new(dir))
        .into_iter()
        .filter_map(|path| path.file_name().map(|n| n.to_string_lossy().into_owned()))
        .collect()
}

fn lockdir_intent_files(dir: &str, execution_ids: &[impl AsRef<str>]) -> Vec<String> {
    lockdir_files(dir)
        .into_iter()
        .filter(|name| name.starts_with("intent-") && name.ends_with(".json"))
        .filter(|name| execution_ids.iter().any(|id| name.contains(id.as_ref())))
        .collect()
}

fn lockdir_ws_dir(dir: &str) -> PathBuf {
    // `record_execution_intent` created exactly one workspace sub-directory
    // (the sha256 of the canonical root) inside the lock dir.
    std::fs::read_dir(dir)
        .expect("lock dir")
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.is_dir())
        .expect("the workspace lock directory exists")
}

// ---------------------------------------------------------------------------
// (A) In-process coordinator: a memory-only quarantine never retires the
//     durable intent it stands in for.
// ---------------------------------------------------------------------------

/// Reviewer reproduction A: first acquire blocks, the quarantine is present,
/// the coordinator is DROPPED (the in-memory evidence dies with it), and a
/// fresh coordinator must still refuse modifying work — `second_admitted`
/// must be false. Before the fix the intent had been deleted at quarantine
/// time, so the reopened coordinator admitted the write.
#[tokio::test(flavor = "multi_thread")]
async fn dropping_the_coordinator_cannot_discard_unresolved_evidence() {
    let _serial = gate().lock().await;
    let workspace = tempfile::tempdir().expect("workspace");
    let root = workspace.path().to_string_lossy().into_owned();
    let execution_id = format!("droprepro-{}", uuid::Uuid::new_v4().simple());

    let first = create_in_process_workspace_coordinator(None);
    first
        .record_execution_intent(&corrupt_intent(&root, &execution_id))
        .await
        .expect("durable unknown-ownership intent publishes");
    let error = acquire(first.as_ref(), &root, "next-run", 200)
        .await
        .expect_err("the unresolved ownership record blocks modifying work (first_blocked)");
    assert_eq!(error.code, "workspace_quarantined", "{error:?}");
    assert!(
        first
            .workspace_quarantine(&root)
            .await
            .expect("readable")
            .is_some(),
        "quarantine_present_before_drop"
    );
    assert_eq!(
        journal_intents(&[&execution_id]).len(),
        1,
        "the durable intent is the only evidence a reopen can reconcile from"
    );

    // Full teardown: the ONLY quarantine was in memory, so the durable
    // intent must have been kept — a fresh coordinator has to see it.
    drop(first);
    let reopened = create_in_process_workspace_coordinator(None);
    let second_admitted = acquire(reopened.as_ref(), &root, "third-run", 200)
        .await
        .is_ok();
    assert!(
        !second_admitted,
        "dropping the coordinator must not admit modifying work over an unresolved execution"
    );
    let error = acquire(reopened.as_ref(), &root, "fourth-run", 200)
        .await
        .expect_err("the reopened coordinator blocks too");
    assert_eq!(error.code, "workspace_quarantined", "{error:?}");
    let edit = acquire_edit(reopened.as_ref(), &root, "editor")
        .await
        .expect_err("the edit path is blocked by the same durable evidence");
    assert_eq!(edit.code, "workspace_quarantined", "{edit:?}");
    assert!(
        reopened
            .workspace_quarantine(&root)
            .await
            .expect("readable")
            .is_some(),
        "the reopen re-establishes the quarantine from the kept intent"
    );

    // Explicit recovery still works — and retires both halves.
    assert!(
        reopened
            .clear_workspace_quarantine(&root)
            .await
            .expect("recovery")
    );
    assert!(
        journal_intents(&[&execution_id]).is_empty(),
        "recovery retires the intent"
    );
    let cleaner = create_in_process_workspace_coordinator(None);
    let mut lease = acquire(cleaner.as_ref(), &root, "recovered", 2_000)
        .await
        .expect("explicit recovery admits the next run");
    lease.release().await;
}

/// Multiple reopen cycles WITHOUT explicit recovery: every fresh coordinator
/// must still block modifying work, and the durable evidence must survive
/// each cycle.
#[tokio::test(flavor = "multi_thread")]
async fn reopen_cycles_keep_blocking_without_explicit_recovery() {
    let _serial = gate().lock().await;
    let workspace = tempfile::tempdir().expect("workspace");
    let root = workspace.path().to_string_lossy().into_owned();
    let execution_id = format!("reopens-{}", uuid::Uuid::new_v4().simple());

    let first = create_in_process_workspace_coordinator(None);
    first
        .record_execution_intent(&corrupt_intent(&root, &execution_id))
        .await
        .expect("intent publishes");
    drop(first);

    for cycle in 1..=4 {
        let reopened = create_in_process_workspace_coordinator(None);
        let error = acquire(reopened.as_ref(), &root, &format!("run-{cycle}"), 200)
            .await
            .expect_err(&format!("reopen cycle {cycle} still blocks modifying work"));
        assert_eq!(error.code, "workspace_quarantined", "{error:?}");
        assert_eq!(
            journal_intents(&[&execution_id]).len(),
            1,
            "the unresolved intent survives reopen cycle {cycle}"
        );
        drop(reopened);
    }

    let last = create_in_process_workspace_coordinator(None);
    let blocked = acquire(last.as_ref(), &root, "pre-recovery", 200)
        .await
        .expect_err("still blocked before recovery");
    assert_eq!(blocked.code, "workspace_quarantined", "{blocked:?}");
    assert!(
        last.clear_workspace_quarantine(&root)
            .await
            .expect("recovery")
    );
    assert!(
        journal_intents(&[&execution_id]).is_empty(),
        "recovery retires the durable intent"
    );
    let mut lease = acquire(last.as_ref(), &root, "recovered", 2_000)
        .await
        .expect("recovery after repeated reopens admits work");
    lease.release().await;
}

/// Crash-boundary simulation: the durable intent was published but NO
/// replacement was ever established (the host died before reconciling). The
/// reopen must block — never admit — while that intent sits unresolved.
#[tokio::test(flavor = "multi_thread")]
async fn crash_boundary_before_any_replacement_blocks_the_reopen() {
    let _serial = gate().lock().await;
    let workspace = tempfile::tempdir().expect("workspace");
    let root = workspace.path().to_string_lossy().into_owned();
    let execution_id = format!("crashboundary-{}", uuid::Uuid::new_v4().simple());

    // The crashing host: records the pre-effect intent, dies before any
    // acquisition/reconcile ever runs — no quarantine was published, in
    // memory or otherwise.
    let host = create_in_process_workspace_coordinator(None);
    host.record_execution_intent(&corrupt_intent(&root, &execution_id))
        .await
        .expect("intent publishes");
    drop(host);

    let reopened = create_in_process_workspace_coordinator(None);
    let error = acquire(reopened.as_ref(), &root, "next-run", 200)
        .await
        .expect_err("intent present + replacement never published = block");
    assert_eq!(error.code, "workspace_quarantined", "{error:?}");
    assert_eq!(
        journal_intents(&[&execution_id]).len(),
        1,
        "reconcile must not retire the intent without durable replacement evidence"
    );
    assert!(
        reopened
            .clear_workspace_quarantine(&root)
            .await
            .expect("recovery")
    );
    let mut lease = acquire(reopened.as_ref(), &root, "recovered", 2_000)
        .await
        .expect("recovery admits work again");
    lease.release().await;
}

/// Multiple unresolved intents on one root: an existing quarantine must not
/// cause LATER unresolved execution identities to be discarded — each
/// intent the quarantine does not stand in for stays durable, across
/// reopens, until recovery retires it.
#[tokio::test(flavor = "multi_thread")]
async fn every_unresolved_intent_survives_an_existing_quarantine() {
    let _serial = gate().lock().await;
    let workspace = tempfile::tempdir().expect("workspace");
    let root = workspace.path().to_string_lossy().into_owned();
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let ids: Vec<String> = ["a", "b", "c"]
        .iter()
        .map(|tag| format!("multi-{nonce}-{tag}"))
        .collect();

    let host = create_in_process_workspace_coordinator(None);
    for id in &ids {
        host.record_execution_intent(&corrupt_intent(&root, id))
            .await
            .expect("intent publishes");
    }
    drop(host);

    for cycle in 1..=2 {
        let reopened = create_in_process_workspace_coordinator(None);
        let error = acquire(reopened.as_ref(), &root, &format!("run-{cycle}"), 200)
            .await
            .expect_err("the unresolved executions block modifying work");
        assert_eq!(error.code, "workspace_quarantined", "{error:?}");
        let kept = journal_intents(&ids);
        assert_eq!(
            kept.len(),
            ids.len(),
            "cycle {cycle}: a root already quarantined must never discard later unresolved identities (kept: {kept:?})"
        );
        drop(reopened);
    }

    let last = create_in_process_workspace_coordinator(None);
    let blocked = acquire(last.as_ref(), &root, "pre-recovery", 200)
        .await
        .expect_err("still blocked before recovery");
    assert_eq!(blocked.code, "workspace_quarantined", "{blocked:?}");
    assert!(
        last.clear_workspace_quarantine(&root)
            .await
            .expect("recovery")
    );
    assert!(
        journal_intents(&ids).is_empty(),
        "recovery retires every unresolved intent"
    );
    let mut lease = acquire(last.as_ref(), &root, "recovered", 2_000)
        .await
        .expect("recovery across all preserved identities admits work");
    lease.release().await;
}

// ---------------------------------------------------------------------------
// (B) Lock-dir coordinator: replace-then-retire, never retire-then-replace.
// ---------------------------------------------------------------------------

/// Reviewer reproduction B: force a quarantine READ failure (a directory
/// under the record's name). Reconciliation must return the IO error with
/// the ORIGINAL INTENT STILL ON DISK (`intent_count_after_error == 1`), and
/// the next coordinator must refuse work (`next_admitted == false`).
/// Before the fix the intent was deleted before the read, so the failed
/// read left neither evidence and the next writer was admitted.
#[tokio::test(flavor = "multi_thread")]
async fn failed_quarantine_read_preserves_the_intent_and_blocks() {
    let _serial = gate().lock().await;
    let workspace = tempfile::tempdir().expect("workspace");
    let lockdir = tempfile::tempdir().expect("lockdir");
    let root = workspace.path().to_string_lossy().into_owned();
    let dir = lockdir.path().to_string_lossy().into_owned();
    let options = || LockDirCoordinatorOptions {
        dir: dir.clone(),
        observer: None,
        poll_ms: Some(20),
        on_error: None,
    };
    let execution_id = format!("readfail-{}", uuid::Uuid::new_v4().simple());

    let first = create_lockdir_workspace_coordinator(options());
    first
        .record_execution_intent(&corrupt_intent(&root, &execution_id))
        .await
        .expect("durable intent publishes");

    // Inject the read failure: a directory under the record's name makes
    // every `read_to_string` of it an IO error, exactly the failure the
    // reviewer forced during recovery.
    let ws = lockdir_ws_dir(&dir);
    std::fs::create_dir(format!("{}/quarantine.json", ws.display()))
        .expect("block the record path");

    let error = first
        .reconcile_execution_intents(&root)
        .await
        .expect_err("a failed quarantine read is surfaced, never swallowed");
    assert_eq!(error.code, "io_error", "{error:?}");
    assert_eq!(
        lockdir_intent_files(&dir, &[&execution_id]).len(),
        1,
        "intent_count_after_error: the original intent must survive the failed read"
    );

    let reopened = create_lockdir_workspace_coordinator(options());
    let next_admitted = acquire(reopened.as_ref(), &root, "next-run", 200)
        .await
        .is_ok();
    assert!(
        !next_admitted,
        "a root whose recovery could not read the quarantine is never admitted (fail closed)"
    );
    let error = acquire(reopened.as_ref(), &root, "third-run", 200)
        .await
        .expect_err("the reopen refuses too while the record is unreadable");
    assert_eq!(error.code, "io_error", "{error:?}");
    assert_eq!(
        lockdir_intent_files(&dir, &[&execution_id]).len(),
        1,
        "the failed acquisitions must not retire the intent either"
    );

    // Remove the injection: with the record path clear, the kept intent
    // publishes its replacement and the root blocks on real evidence.
    std::fs::remove_dir(format!("{}/quarantine.json", ws.display())).expect("clear injection");
    let error = acquire(reopened.as_ref(), &root, "fourth-run", 200)
        .await
        .expect_err("the published quarantine blocks modifying work");
    assert_eq!(error.code, "workspace_quarantined", "{error:?}");
    assert!(
        lockdir_files(&dir)
            .iter()
            .any(|name| name == "quarantine.json"),
        "the replacement quarantine is durable"
    );
    assert!(
        reopened
            .clear_workspace_quarantine(&root)
            .await
            .expect("recovery")
    );
    let mut lease = acquire(reopened.as_ref(), &root, "recovered", 2_000)
        .await
        .expect("explicit recovery admits work again");
    lease.release().await;
}

/// Failed quarantine PUBLICATION (temp-file creation in the workspace lock
/// directory fails): the reconcile must surface the IO error, PRESERVE the
/// original intent, and keep blocking. Skipped when running as root, where
/// directory permissions cannot gate the publication.
#[tokio::test(flavor = "multi_thread")]
async fn failed_quarantine_publication_preserves_the_intent_and_blocks() {
    let _serial = gate().lock().await;
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("running as root: permission injection is inert, skipping");
        return;
    }
    let workspace = tempfile::tempdir().expect("workspace");
    let lockdir = tempfile::tempdir().expect("lockdir");
    let root = workspace.path().to_string_lossy().into_owned();
    let dir = lockdir.path().to_string_lossy().into_owned();
    let options = || LockDirCoordinatorOptions {
        dir: dir.clone(),
        observer: None,
        poll_ms: Some(20),
        on_error: None,
    };
    let execution_id = format!("pubfail-{}", uuid::Uuid::new_v4().simple());

    let first = create_lockdir_workspace_coordinator(options());
    first
        .record_execution_intent(&corrupt_intent(&root, &execution_id))
        .await
        .expect("durable intent publishes");

    // Inject the publication failure: the quarantine read succeeds (no
    // record), but the atomic temp file for the replacement cannot be
    // created in a directory that no longer accepts writes.
    let ws = lockdir_ws_dir(&dir);
    std::fs::set_permissions(&ws, std::fs::Permissions::from_mode(0o555))
        .expect("make the lock directory read-only");
    let error = first
        .reconcile_execution_intents(&root)
        .await
        .expect_err("a failed publication is surfaced, never swallowed");
    assert_eq!(error.code, "io_error", "{error:?}");
    assert_eq!(
        lockdir_intent_files(&dir, &[&execution_id]).len(),
        1,
        "a quarantine that could not be published must not retire its intent"
    );

    let next_admitted = acquire(first.as_ref(), &root, "next-run", 200)
        .await
        .is_ok();
    assert!(
        !next_admitted,
        "no durable replacement, no admission: the kept intent keeps the root closed"
    );

    // Restore writability: the preserved intent now publishes its
    // replacement and the block becomes real evidence, not just an error.
    std::fs::set_permissions(&ws, std::fs::Permissions::from_mode(0o755))
        .expect("restore the lock directory");
    let reopened = create_lockdir_workspace_coordinator(options());
    let error = acquire(reopened.as_ref(), &root, "fourth-run", 200)
        .await
        .expect_err("the delayed replacement still blocks");
    assert_eq!(error.code, "workspace_quarantined", "{error:?}");
    assert!(
        lockdir_files(&dir)
            .iter()
            .any(|name| name == "quarantine.json"),
        "the replacement quarantine is durable"
    );
    assert!(
        reopened
            .clear_workspace_quarantine(&root)
            .await
            .expect("recovery")
    );
    let mut lease = acquire(reopened.as_ref(), &root, "recovered", 2_000)
        .await
        .expect("explicit recovery admits work again");
    lease.release().await;
}

/// Lock-dir reopen cycles without explicit recovery: each fresh coordinator
/// over the same durable directory re-blocks the root, and the intent keeps
/// the root closed until recovery.
#[tokio::test(flavor = "multi_thread")]
async fn lockdir_reopen_cycles_keep_blocking_without_explicit_recovery() {
    let _serial = gate().lock().await;
    let workspace = tempfile::tempdir().expect("workspace");
    let lockdir = tempfile::tempdir().expect("lockdir");
    let root = workspace.path().to_string_lossy().into_owned();
    let dir = lockdir.path().to_string_lossy().into_owned();
    let options = || LockDirCoordinatorOptions {
        dir: dir.clone(),
        observer: None,
        poll_ms: Some(20),
        on_error: None,
    };
    let execution_id = format!("ldreopens-{}", uuid::Uuid::new_v4().simple());

    let host = create_lockdir_workspace_coordinator(options());
    host.record_execution_intent(&corrupt_intent(&root, &execution_id))
        .await
        .expect("intent publishes");
    drop(host); // crash: no reconcile ever ran in the owning coordinator

    for cycle in 1..=4 {
        let reopened = create_lockdir_workspace_coordinator(options());
        let error = acquire(reopened.as_ref(), &root, &format!("run-{cycle}"), 200)
            .await
            .expect_err(&format!("lock-dir reopen cycle {cycle} still blocks"));
        assert_eq!(error.code, "workspace_quarantined", "{error:?}");
        assert!(
            lockdir_files(&dir)
                .iter()
                .any(|name| name == "quarantine.json"),
            "the durable replacement persists across reopen cycle {cycle}"
        );
        drop(reopened);
    }

    let last = create_lockdir_workspace_coordinator(options());
    assert!(
        last.clear_workspace_quarantine(&root)
            .await
            .expect("recovery")
    );
    let mut lease = acquire(last.as_ref(), &root, "recovered", 2_000)
        .await
        .expect("recovery after repeated reopens admits work");
    lease.release().await;
}

/// Multiple unresolved lock-dir intents on one root: the first publishes
/// its replacement; every LATER one whose replacement never got published
/// (the existing record already blocks the root) keeps its own durable
/// intent — individually preserved, across reopens — until recovery.
#[tokio::test(flavor = "multi_thread")]
async fn every_unresolved_lockdir_intent_survives_an_existing_quarantine() {
    let _serial = gate().lock().await;
    let workspace = tempfile::tempdir().expect("workspace");
    let lockdir = tempfile::tempdir().expect("lockdir");
    let root = workspace.path().to_string_lossy().into_owned();
    let dir = lockdir.path().to_string_lossy().into_owned();
    let options = || LockDirCoordinatorOptions {
        dir: dir.clone(),
        observer: None,
        poll_ms: Some(20),
        on_error: None,
    };
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let ids: Vec<String> = ["a", "b", "c", "d"]
        .iter()
        .map(|tag| format!("ldmulti-{nonce}-{tag}"))
        .collect();

    let host = create_lockdir_workspace_coordinator(options());
    for id in &ids {
        host.record_execution_intent(&corrupt_intent(&root, id))
            .await
            .expect("intent publishes");
    }
    drop(host);

    for cycle in 1..=2 {
        let reopened = create_lockdir_workspace_coordinator(options());
        let error = acquire(reopened.as_ref(), &root, &format!("run-{cycle}"), 200)
            .await
            .expect_err("unresolved executions block modifying work");
        assert_eq!(error.code, "workspace_quarantined", "{error:?}");
        assert!(
            lockdir_files(&dir)
                .iter()
                .any(|name| name == "quarantine.json"),
            "cycle {cycle}: exactly one durable replacement exists"
        );
        let kept = lockdir_intent_files(&dir, &ids);
        assert_eq!(
            kept.len(),
            ids.len() - 1,
            "cycle {cycle}: only the intent whose replacement was published may go; \
             an existing quarantine must not discard later identities (kept: {kept:?})"
        );
        drop(reopened);
    }

    let last = create_lockdir_workspace_coordinator(options());
    assert!(
        last.clear_workspace_quarantine(&root)
            .await
            .expect("recovery")
    );
    assert!(
        lockdir_intent_files(&dir, &ids).is_empty(),
        "explicit recovery retires the quarantine AND the preserved intents"
    );
    let mut lease = acquire(last.as_ref(), &root, "recovered", 2_000)
        .await
        .expect("recovery across all preserved identities admits work");
    lease.release().await;
}

// ---------------------------------------------------------------------------
// (D) In-process coordinator: a DRIVER-created quarantine is durable.
//     The assessor's reopen defect: `quarantine_execution` recorded only
//     in memory, the driver treated the `Ok` as sufficient replacement
//     evidence, deleted the durable intent — and a fresh coordinator over
//     the same store admitted modifying work while the marked child lived.
// ---------------------------------------------------------------------------

/// The in-process coordinator's durable journal directory for a root
/// (same layout `write_intent_file`/`write_quarantine_file` use).
async fn journal_dir_for(root: &str) -> PathBuf {
    let key = canonical_root_key(root).await;
    std::env::temp_dir()
        .join("agentkit-exec-intents")
        .join(sha256(key.as_bytes()))
}

fn now_wall_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_millis() as u64
}

/// A live child leading its own process group: the recorded `pgid` the
/// quarantine's positive-exit probe consults, with a liveness nobody can
/// forge (kill(-pgid, 0) answers "populated" while it lives).
fn live_group_child() -> (i32, std::process::Child) {
    let child = std::process::Command::new("/bin/sleep")
        .arg("300")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn()
        .expect("spawn group child");
    (child.id() as i32, child)
}

/// The assessor's reopen defect, pinned at the coordinator level: a
/// driver-published command quarantine must survive the coordinator drop
/// AND repeated reopens (durable `quarantine.json` in the intent journal),
/// deny modifying work every time, and lift only on positive exit.
#[tokio::test(flavor = "multi_thread")]
async fn in_process_driver_quarantine_survives_reopen_until_positive_exit() {
    let _serial = gate().lock().await;
    let workspace = tempfile::tempdir().expect("workspace");
    let root = workspace.path().to_string_lossy().into_owned();
    let execution_id = format!("durableq-{}", uuid::Uuid::new_v4().simple());
    let (child_pid, mut child) = live_group_child();

    let coordinator = create_in_process_workspace_coordinator(None);
    let identity = CommandCleanupIdentity {
        execution_id: execution_id.clone(),
        owner_pid: i64::from(std::process::id()),
        pgid: Some(child_pid),
        spawned_at_ms: now_wall_ms(),
    };
    coordinator
        .record_execution_intent(&ExecutionIntent {
            root: root.clone(),
            identity: identity.clone(),
            holder: holder("quarantined-run"),
            spawned: true,
        })
        .await
        .expect("intent publishes");
    coordinator
        .quarantine_execution(
            &root,
            &QuarantineEvidence {
                pids: vec![child_pid],
                holder: holder("quarantined-run"),
            },
            &identity,
        )
        .await
        .expect("command quarantine publishes durably");

    let journal = journal_dir_for(&root).await;
    assert!(
        journal.join("quarantine.json").exists(),
        "the driver quarantine has a durable record: {}",
        journal.display()
    );
    assert_eq!(
        journal_intents(&[&execution_id]).len(),
        1,
        "the durable intent is retained alongside the quarantine"
    );

    // Repeated reopens over the SAME store deny modifying work.
    drop(coordinator);
    for cycle in 1..=3 {
        let reopened = create_in_process_workspace_coordinator(None);
        let error = acquire(reopened.as_ref(), &root, &format!("run-{cycle}"), 200)
            .await
            .expect_err(&format!("in-process reopen cycle {cycle} still blocks"));
        assert_eq!(error.code, "workspace_quarantined", "{error:?}");
        assert!(
            journal.join("quarantine.json").exists(),
            "denial never consumes the durable record (cycle {cycle})"
        );
        drop(reopened);
    }

    // Positive exit: the recorded process dies → the probe settles → the
    // quarantine lifts on evidence and work is admitted again.
    let _ = child.kill();
    let _ = child.wait();
    let last = create_in_process_workspace_coordinator(None);
    let mut lease = acquire(last.as_ref(), &root, "after-exit", 2_000)
        .await
        .expect("positive exit lifts the durable quarantine");
    lease.release().await;
    assert!(
        !journal.join("quarantine.json").exists(),
        "the lifted quarantine retires its durable record"
    );
    let _ = std::fs::remove_dir_all(&journal);
}

/// A failed durable publication of the quarantine is an `Err` — and the
/// ORIGINAL evidence (the durable intent) is preserved, fail-closed: the
/// root keeps blocking in memory, nothing is half-retired, and once the
/// journal is writable again the quarantine publishes durably.
#[tokio::test(flavor = "multi_thread")]
async fn failed_in_process_quarantine_publication_keeps_original_evidence() {
    let _serial = gate().lock().await;
    let workspace = tempfile::tempdir().expect("workspace");
    let root = workspace.path().to_string_lossy().into_owned();
    let execution_id = format!("qpubfail-{}", uuid::Uuid::new_v4().simple());
    let (child_pid, mut child) = live_group_child();

    let coordinator = create_in_process_workspace_coordinator(None);
    let identity = CommandCleanupIdentity {
        execution_id: execution_id.clone(),
        owner_pid: i64::from(std::process::id()),
        pgid: Some(child_pid),
        spawned_at_ms: now_wall_ms(),
    };
    coordinator
        .record_execution_intent(&ExecutionIntent {
            root: root.clone(),
            identity: identity.clone(),
            holder: holder("quarantined-run"),
            spawned: true,
        })
        .await
        .expect("intent publishes");
    let journal = journal_dir_for(&root).await;

    // Inject: journal not writable → quarantine publication MUST fail.
    std::fs::set_permissions(&journal, std::fs::Permissions::from_mode(0o500))
        .expect("make journal read-only");
    let error = coordinator
        .quarantine_execution(
            &root,
            &QuarantineEvidence {
                pids: vec![child_pid],
                holder: holder("quarantined-run"),
            },
            &identity,
        )
        .await
        .expect_err("unwritable journal fails the durable publication");
    assert_eq!(error.code, "io_error", "{error:?}");
    assert_eq!(
        journal_intents(&[&execution_id]).len(),
        1,
        "the original durable evidence survives the failed publication"
    );
    // Fail closed: the failed publication still blocks the root.
    let blocked = acquire(coordinator.as_ref(), &root, "blocked", 200)
        .await
        .expect_err("failed quarantine publication keeps the root blocked");
    assert_eq!(blocked.code, "workspace_quarantined", "{blocked:?}");

    // Restore writability: the quarantine now publishes durably, and a
    // REOPENED coordinator still denies the next modifying lease.
    std::fs::set_permissions(&journal, std::fs::Permissions::from_mode(0o755))
        .expect("restore journal");
    coordinator
        .quarantine_execution(
            &root,
            &QuarantineEvidence {
                pids: vec![child_pid],
                holder: holder("quarantined-run"),
            },
            &identity,
        )
        .await
        .expect("quarantine publishes once the journal is writable");
    assert!(journal.join("quarantine.json").exists());
    drop(coordinator);
    let reopened = create_in_process_workspace_coordinator(None);
    let error = acquire(reopened.as_ref(), &root, "next-writer", 200)
        .await
        .expect_err("the published quarantine blocks modifying work");
    assert_eq!(error.code, "workspace_quarantined", "{error:?}");

    // Explicit recovery resolves the state (and retires the durable
    // record); the live child is reaped on every path.
    let last = create_in_process_workspace_coordinator(None);
    assert!(
        last.clear_workspace_quarantine(&root)
            .await
            .expect("recovery"),
        "recovery reports the durable record it retired"
    );
    let mut lease = acquire(last.as_ref(), &root, "recovered", 2_000)
        .await
        .expect("explicit recovery admits work");
    lease.release().await;
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&journal);
}
