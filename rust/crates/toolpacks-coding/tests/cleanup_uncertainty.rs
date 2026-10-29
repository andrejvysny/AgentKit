//! Execution-cleanup uncertainty + quarantine durability (assessor
//! reproduction, WITHOUT prctl).
//!
//! The assessor reproduced: a marked child detaches into its own process
//! group with an unreadable environ (`PR_SET_DUMPABLE=0` →
//! `PermissionDenied` on `/proc/<pid>/environ`); the detailed `/proc` scan
//! classifies it as a SUSPECT; the live discovery path DROPPED the suspect
//! set; `survivors=[]` then read as successful cleanup — the intent was
//! retired, the lease released, and the next modifying lease was admitted
//! while the child was still alive.
//!
//! Regression contract pinned here:
//!   * the discovery closure feeds the FULL outcome (marked + suspects +
//!     unscannable) into every cleanup decision — `spawn_owned_execution`
//!     takes it as an injection seam, so this test reproduces the exact
//!     scan verdict ({no survivors, suspect child alive, environ denied})
//!     deterministically, no prctl and no scheduler race;
//!   * cleanup is reported UNCERTAIN, a quarantine is established with the
//!     safe identity, and the DURABLE intent is RETAINED (not deleted);
//!   * a reopened coordinator over the same store DENIES the next
//!     modifying lease with the established `workspace_quarantined`
//!     uncertainty error;
//!   * when the suspect positively exits, the quarantine lifts and work is
//!     admitted again (no wedge without a recovery path).
//!
//! Style/serialization follow `tests/coordination_durability.rs`: one
//! mutex gate, RecoveryStore-equivalent fixtures = the in-process
//! coordinator's durable intent journal under the system temp dir; every
//! subprocess is reaped on every exit path.

#![cfg(unix)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use agentkit_toolpacks_coding::coordination::{
    AcquireFileInput, AcquireWorkspaceInput, CommandCleanupIdentity, ExecutionIntent, LeaseHolder,
    WorkspaceCoordinator, canonical_root_key, create_in_process_workspace_coordinator,
};
use agentkit_toolpacks_coding::fs_util::sha256;
use agentkit_toolpacks_coding::limits::CodingLimits;
use agentkit_toolpacks_coding::process::{
    CleanupOutcome, Discovery, FindFn, OwnedRunOutcome, RunSpec,
    spawn_owned_execution_with_discovery,
};

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

fn pid_alive(pid: i32) -> bool {
    pid > 0 && unsafe { libc::kill(pid, 0) } == 0
}

fn now_wall_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_millis() as u64
}

/// A live child a quarantine may name as its suspect, killed and reaped on
/// EVERY exit path (mirrors `ChildGuard` in the durability suite).
struct ChildGuard(Option<std::process::Child>);

impl ChildGuard {
    fn spawn_detached(program: &str, args: &[&str]) -> (i32, Self) {
        let child = std::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0)
            .spawn()
            .expect("spawn child");
        let pid = child.id() as i32;
        (pid, Self(Some(child)))
    }

    fn reap(mut self) {
        let Some(mut child) = self.0.take() else {
            return;
        };
        let pid = child.id() as i32;
        let _ = child.kill();
        let _ = child.wait();
        assert!(!pid_alive(pid), "child {pid} still alive after kill+wait");
    }

    fn spawn_marked(execution_id: &str) -> (i32, Self) {
        let child = std::process::Command::new("/bin/sleep")
            .arg("300")
            .env("AGENTKIT_EXEC_ID", execution_id)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0)
            .spawn()
            .expect("spawn marked child");
        (child.id() as i32, Self(Some(child)))
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

use std::os::unix::process::CommandExt;

/// The in-process coordinator's durable journal directory for a root:
/// `$TMPDIR/agentkit-exec-intents/<sha256(canonical root)>`.
async fn journal_dir(root: &str) -> PathBuf {
    let key = canonical_root_key(root).await;
    std::env::temp_dir()
        .join("agentkit-exec-intents")
        .join(sha256(key.as_bytes()))
}

/// The journal file name for an execution nonce (same sanitizing as the
/// coordinator's `intent_file_name`).
fn intent_file(execution_id: &str) -> String {
    execution_id.replace([':', '/', '\\', ' '], "-") + ".json"
}

async fn acquire(
    coordinator: &dyn WorkspaceCoordinator,
    root: &str,
    run: &str,
    max_wait_ms: u64,
) -> Result<
    agentkit_toolpacks_coding::coordination::WorkspaceLease,
    agentkit_toolpacks_coding::errors::CodingError,
> {
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
) -> Result<
    agentkit_toolpacks_coding::coordination::WorkspaceLease,
    agentkit_toolpacks_coding::errors::CodingError,
> {
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

async fn wait_until<F: FnMut() -> bool>(mut done: F, seconds: u64, what: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
    loop {
        if done() {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for: {what}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// A process whose environment becomes readable between discovery passes
/// is still ours when that read exposes the execution marker. The public
/// driver must retain it as a survivor and block the next modifying lease.
#[tokio::test(flavor = "multi_thread")]
async fn suspect_reclassified_as_marked_stays_blocking() {
    let _serial = gate().lock().await;
    let workspace = tempfile::tempdir().expect("workspace");
    let root = workspace.path().to_string_lossy().into_owned();
    let execution_id = format!("{}:{}", std::process::id(), uuid::Uuid::new_v4().simple());
    let (pid, child) = ChildGuard::spawn_marked(&execution_id);
    let coordinator = create_in_process_workspace_coordinator(None);
    coordinator
        .record_execution_intent(&ExecutionIntent {
            root: root.clone(),
            identity: CommandCleanupIdentity {
                execution_id: execution_id.clone(),
                owner_pid: i64::from(std::process::id()),
                pgid: None,
                spawned_at_ms: now_wall_ms(),
            },
            holder: holder("reclassified"),
            spawned: false,
        })
        .await
        .unwrap();
    let lease = acquire(coordinator.as_ref(), &root, "reclassified", 2_000)
        .await
        .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let find: FindFn = Arc::new(move || {
        let first = counter.fetch_add(1, Ordering::SeqCst) == 0;
        Box::pin(async move {
            Some(if first {
                Discovery {
                    marked: vec![],
                    suspects: vec![pid],
                }
            } else {
                Discovery {
                    marked: vec![pid],
                    suspects: vec![],
                }
            })
        })
    });
    let outcome = spawn_owned_execution_with_discovery(
        RunSpec {
            file: "/bin/sh".into(),
            args: vec!["-c".into(), "true".into()],
            cwd: root.clone(),
            env: Default::default(),
            timeout_ms: 15_000,
            cancellation: None,
            scratch: root.clone(),
            log_name: "reclassified.log".into(),
            limits: CodingLimits::default(),
            execution_id: execution_id.clone(),
            pgid_slot: None,
        },
        lease,
        Arc::clone(&coordinator),
        root.clone(),
        holder("reclassified"),
        Some(find),
    )
    .await_result()
    .await;
    assert!(calls.load(Ordering::SeqCst) >= 2);
    assert!(pid_alive(pid), "this late discovery has not been signalled");
    assert!(outcome.cleanup.uncertainty().survivors.contains(&pid));
    assert!(outcome.quarantine_error.is_none());
    let journal = journal_dir(&root).await;
    assert!(journal.join("quarantine.json").exists());
    assert!(journal.join(intent_file(&execution_id)).exists());

    drop(coordinator);
    let reopened = create_in_process_workspace_coordinator(None);
    let error = acquire(reopened.as_ref(), &root, "next-writer", 200)
        .await
        .expect_err("the still-live marked process must block the next lease");
    assert_eq!(error.code, "workspace_quarantined");
    child.reap();
    let _ = std::fs::remove_dir_all(journal);
}

/// Deterministic reproduction of the assessor scenario via the discovery
/// injection seam: the scan reports NO survivors, ONE live suspect child,
/// and (by the suspect classification itself) environ PermissionDenied.
/// The driver must: report cleanup Uncertain, quarantine with the safe
/// identity, and RETAIN the durable intent — so a reopened coordinator
/// over the same store denies the next modifying lease.
#[tokio::test(flavor = "multi_thread")]
async fn closure_injected_uncertain_cleanup_quarantines_and_retains_intent() {
    let _serial = gate().lock().await;
    let workspace = tempfile::tempdir().expect("workspace");
    let root = workspace.path().to_string_lossy().into_owned();

    // The live "suspect": a real child that outlives the command, standing
    // in for the marked child whose environ returned PermissionDenied.
    let (suspect_pid, suspect_guard) = ChildGuard::spawn_detached("/bin/sleep", &["300"]);
    assert!(pid_alive(suspect_pid));

    let coordinator = create_in_process_workspace_coordinator(None);
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let execution_id = format!("{}:{nonce}", std::process::id());

    // Same pre-spawn durable intent the real command path writes BEFORE
    // admission (B5): this is the record the buggy driver deleted.
    coordinator
        .record_execution_intent(&ExecutionIntent {
            root: root.clone(),
            identity: CommandCleanupIdentity {
                execution_id: execution_id.clone(),
                owner_pid: i64::from(std::process::id()),
                pgid: None,
                spawned_at_ms: now_wall_ms(),
            },
            holder: holder("uncertain-run"),
            spawned: false,
        })
        .await
        .expect("intent publishes");
    let lease = acquire(coordinator.as_ref(), &root, "uncertain-run", 2_000)
        .await
        .expect("clean root admits the command");

    // The injected discovery: exactly the assessor's detailed-scan verdict
    // — marks empty, the detached child classified SUSPECT (environ
    // PermissionDenied), scan itself completed.
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_inner = Arc::clone(&calls);
    let suspect = suspect_pid;
    let find: FindFn = Arc::new(move || {
        calls_inner.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            Some(Discovery {
                marked: vec![],
                suspects: vec![suspect],
            })
        })
    });

    let execution = spawn_owned_execution_with_discovery(
        RunSpec {
            file: "/bin/sh".to_owned(),
            args: vec!["-c".to_owned(), "true".to_owned()],
            cwd: root.clone(),
            env: std::collections::HashMap::new(),
            timeout_ms: 15_000,
            cancellation: None,
            scratch: root.clone(),
            log_name: "inject.log".to_owned(),
            limits: CodingLimits::default(),
            execution_id: execution_id.clone(),
            pgid_slot: None,
        },
        lease,
        Arc::clone(&coordinator),
        root.clone(),
        holder("uncertain-run"),
        Some(find),
    );
    let outcome: OwnedRunOutcome = execution.await_result().await;

    // 1. The injected closure actually drove cleanup.
    assert!(
        calls.load(Ordering::SeqCst) > 0,
        "discovery closure consumed"
    );

    // 2. Cleanup reported UNCERTAIN — with the complete reason set, not an
    //    empty-survivor "success".
    assert!(
        !outcome.cleanup.is_clean(),
        "a live suspect must never read as clean: {:?}",
        outcome.cleanup
    );
    match &outcome.cleanup {
        CleanupOutcome::Uncertain(u) => {
            assert_eq!(u.suspects, vec![suspect], "suspect reason preserved");
            assert!(u.survivors.is_empty());
            assert!(!u.scan_unsupported && !u.group_stuck);
        }
        CleanupOutcome::Clean => unreachable!("asserted above"),
    }
    assert!(outcome.stragglers.is_empty());
    assert!(outcome.quarantine_error.is_none());

    // 3. Quarantine established with the SAFE identity, naming the suspect.
    let quarantine = coordinator
        .workspace_quarantine(&root)
        .await
        .expect("quarantine readable")
        .expect("uncertain cleanup quarantines the root");
    assert!(
        quarantine.pids.contains(&suspect),
        "the suspect is named in the evidence: {:?}",
        quarantine.pids
    );
    let identity = quarantine
        .identity
        .expect("command quarantine carries the identity");
    assert_eq!(identity.execution_id, execution_id);

    // 4. DURABLE evidence: the quarantine record AND the retained intent
    //    are both on disk in the root's journal.
    let journal = journal_dir(&root).await;
    assert!(
        journal.join("quarantine.json").exists(),
        "driver-created quarantine must be durable: {}",
        journal.display()
    );
    assert!(
        journal.join(intent_file(&execution_id)).exists(),
        "the durable intent is RETAINED while cleanup stays uncertain"
    );

    // 5. Reopen over the SAME store: the next modifying lease is DENIED
    //    with the established uncertainty error.
    drop(coordinator);
    let reopened = create_in_process_workspace_coordinator(None);
    let error = acquire(reopened.as_ref(), &root, "next-writer", 200)
        .await
        .expect_err("reopen must deny modifying work");
    assert_eq!(error.code, "workspace_quarantined", "{error:?}");
    let edit = acquire_edit(reopened.as_ref(), &root, "next-editor")
        .await
        .expect_err("reopen must deny edits too");
    assert_eq!(edit.code, "workspace_quarantined", "{edit:?}");
    assert!(
        journal.join("quarantine.json").exists(),
        "denial without recovery must not consume the durable record"
    );

    // 6. The suspect positively exits → the quarantine lifts on evidence
    //    and modifying work is admitted again (no permanent wedge).
    suspect_guard.reap();
    wait_until(|| !pid_alive(suspect_pid), 10, "suspect exits").await;
    let last = create_in_process_workspace_coordinator(None);
    let mut lease = acquire(last.as_ref(), &root, "after-exit", 2_000)
        .await
        .expect("positive exit lifts the quarantine");
    lease.release().await;

    // Journal hygiene for this disposable root (the retained intent dies
    // with the test workspace).
    let _ = std::fs::remove_dir_all(&journal);
}

/// End-to-end, no injection, real `/proc` scans (Linux-only): a command
/// spawns a MARKED child that detaches via `setsid` into its own session
/// and outlives the command's shell. The detailed two-pass scan must
/// classify it, teardown must take it down through the marker (not the
/// group net — it left the group), and cleanup must NOT report clean:
/// a group member the kill could not remove (here a zombie the group
/// probe still sees as populated — kill(-pgid, 0) answers 0) is a
/// kill-failure uncertainty, never a success. Deterministic throughout:
/// every wait is a poll with a deadline, no sleep-as-sync.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn detached_marked_child_is_classified_and_cleanup_not_clean() {
    let _serial = gate().lock().await;
    let workspace = tempfile::tempdir().expect("workspace");
    let root = workspace.path().to_string_lossy().into_owned();

    let coordinator = create_in_process_workspace_coordinator(None);
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let execution_id = format!("{}:{nonce}", std::process::id());
    coordinator
        .record_execution_intent(&ExecutionIntent {
            root: root.clone(),
            identity: CommandCleanupIdentity {
                execution_id: execution_id.clone(),
                owner_pid: i64::from(std::process::id()),
                pgid: None,
                spawned_at_ms: now_wall_ms(),
            },
            holder: holder("detached-run"),
            spawned: false,
        })
        .await
        .expect("intent publishes");
    let lease = acquire(coordinator.as_ref(), &root, "detached-run", 2_000)
        .await
        .expect("clean root admits the command");

    let limits = CodingLimits {
        command_kill_grace_ms: 300,
        ..CodingLimits::default()
    };
    let execution = spawn_owned_execution_with_discovery(
        RunSpec {
            file: "/bin/sh".to_owned(),
            args: vec![
                "-c".to_owned(),
                // Detach a marked sleeper into its own session, then stay
                // alive briefly so the test can plant its group witness
                // while the group leader still leads.
                "setsid /bin/sleep 60 & sleep 2".to_owned(),
            ],
            cwd: root.clone(),
            env: std::collections::HashMap::new(),
            timeout_ms: 20_000,
            cancellation: None,
            scratch: root.clone(),
            log_name: "detached.log".to_owned(),
            limits,
            execution_id: execution_id.clone(),
            pgid_slot: None,
        },
        lease,
        Arc::clone(&coordinator),
        root.clone(),
        holder("detached-run"),
        None, // the REAL discovery: production marker scan
    );

    // Wait (with a deadline, not a fixed sleep) until the driver published
    // the child's process group.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let pgid = loop {
        if let Some(live) = agentkit_toolpacks_coding::process::live_executions()
            .into_iter()
            .find(|live| live.execution_id == execution_id)
            && let Some(p) = live.pgid
        {
            break p;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the driver never published the child's pgid"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    };

    // Plant the group witness: a child of THIS process in the command's
    // group that exits immediately and is never reaped while this process
    // lives. kill(-pgid, 0) keeps answering "populated" for it, so no
    // signal can make cleanup positively established while we hold it.
    let mut witness = std::process::Command::new("/bin/true")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(pgid)
        .spawn()
        .expect("plant group witness");

    // The detailed scan must CLASSIFY the detached marked child (it has
    // already left the group; only the marker can find it).
    let mut classified: Vec<i32> = vec![];
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(scan) = agentkit_toolpacks_coding::process_tree::find_marked_processes_outcome(
            &execution_id,
            Some(now_wall_ms()),
        )
        .await
            && !scan.0.is_empty()
        {
            classified = scan.0;
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the detached marked child was never classified by the detailed scan"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let outcome = execution.await_result().await;
    assert_eq!(outcome.run.exit_code, Some(0));

    // 1. Cleanup must NOT be clean: the group stayed populated through the
    //    whole kill sequence (kill failure), beside whatever the scan saw.
    assert!(
        !outcome.cleanup.is_clean(),
        "cleanup over a live detached child must never report clean: {:?}",
        outcome.cleanup
    );
    let uncertainty = outcome.cleanup.uncertainty();
    assert!(
        uncertainty.group_stuck || !uncertainty.survivors.is_empty(),
        "the verdict must name the kill failure: {uncertainty:?}"
    );

    // 2. Quarantine with the safe identity; durable evidence both halves.
    let quarantine = coordinator
        .workspace_quarantine(&root)
        .await
        .expect("quarantine readable")
        .expect("uncertain cleanup quarantines the root");
    let identity = quarantine.identity.expect("command identity carried");
    assert_eq!(identity.execution_id, execution_id);
    let journal = journal_dir(&root).await;
    assert!(
        journal.join("quarantine.json").exists(),
        "driver quarantine must be durable: {}",
        journal.display()
    );
    assert!(
        journal.join(intent_file(&execution_id)).exists(),
        "the durable intent is retained while cleanup stays uncertain"
    );

    // 3. Reopen: modifying work denied while the group is still populated.
    drop(coordinator);
    let reopened = create_in_process_workspace_coordinator(None);
    let error = acquire(reopened.as_ref(), &root, "next-writer", 200)
        .await
        .expect_err("reopen must deny modifying work");
    assert_eq!(error.code, "workspace_quarantined", "{error:?}");

    // 4. The marker kill DID take the detached child down: the scan now
    //    finds nothing carrying the nonce (honest kill, not fake-clean:
    //    the block above came from the group probe, not a swallowed scan).
    let scan = agentkit_toolpacks_coding::process_tree::find_marked_processes_outcome(
        &execution_id,
        Some(now_wall_ms()),
    )
    .await
    .expect("linux scans");
    assert!(
        scan.0.is_empty(),
        "detached marked children {} should have been terminated",
        classified
            .iter()
            .map(i32::to_string)
            .collect::<Vec<_>>()
            .join(",")
    );

    // 5. Reap the witness → the group is positively empty → the quarantine
    //    lifts on evidence and modifying work is admitted.
    let _ = witness.wait();
    let last = create_in_process_workspace_coordinator(None);
    let mut lease = acquire(last.as_ref(), &root, "after-exit", 2_000)
        .await
        .expect("positive group exit lifts the quarantine");
    lease.release().await;

    let _ = std::fs::remove_dir_all(&journal);
}
