//! B5 (restart / cleanup uncertainty): real process-kill / reopen tests at
//! the admit → effect → completion boundary.
//!
//! A controlled child is spawned (confined to a disposable temp workspace),
//! the owning "host" process is SIGKILLed — the crash that skips every
//! teardown — and a FRESH coordinator instance (the reopen) must classify
//! the effect: a live process group keeps the workspace blocked as UNCERTAIN
//! evidence naming the exact execution nonce until it is positively gone or
//! recovery is explicitly recorded, and nothing is ever automatically
//! replayed. SAFE process identity only: stable execution nonce + owner pid
//! + process group + spawn time — never a bare pid (pids get recycled).
//!
//! Every test in this file serializes behind one mutex: startup orphan
//! sweeps kill marked processes machine-wide whose owner pid is dead, so a
//! crashed-but-live fixture in one test must never overlap another test's
//! acquisition. Every subprocess is reaped on every exit path.

#![cfg(unix)]

use std::os::unix::process::CommandExt;
use std::sync::{Arc, LazyLock};

use agentkit_toolpacks_coding::coordination::{
    AcquireFileInput, AcquireWorkspaceInput, CommandCleanupIdentity, ExecutionIntent, LeaseHolder,
    WorkspaceCoordinator, create_in_process_workspace_coordinator,
};
use agentkit_toolpacks_coding::coordination_lockdir::{
    LockDirCoordinatorOptions, create_lockdir_workspace_coordinator,
};

static GATE: LazyLock<tokio::sync::Mutex<()>> = LazyLock::new(|| tokio::sync::Mutex::new(()));

// macOS hides the environment of Apple-signed sleep; this fixture remains
// visible to the actual marker scan used by recovery.
#[test]
#[ignore]
fn marked_fixture() {
    std::thread::sleep(std::time::Duration::from_secs(300));
}

fn marked_command() -> std::process::Command {
    let mut command = std::process::Command::new(std::env::current_exe().expect("test executable"));
    command.args(["--ignored", "--exact", "marked_fixture"]);
    command
}

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

async fn wait_until_gone(pid: i32, seconds: u64) -> bool {
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(seconds);
    loop {
        if !pid_alive(pid) {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(25)).await;
    }
}

/// Construct the durable intent the pre-crash host records.
///
/// The owner pid appears twice with two different roles, deliberately split
/// for the test: the execution id — what the marked child carries in
/// `AGENTKIT_EXEC_ID` — names the LIVE test-harness pid so the startup
/// orphan sweep a sibling test binary triggers (`sweep_orphans` runs once
/// per process on the first real command) does not reap the crashed-but-live
/// fixture mid-assertion; the sweep only kills marked processes whose owner
/// pid is dead. `owner_pid` is the actually crashed host the intent records,
/// which is what recovery must observe dead. In production both are the same
/// host pid; nothing there changes.
fn intent(
    root: &str,
    marker_owner_pid: i32,
    owner_pid: i32,
    nonce: &str,
    pgid: Option<i32>,
) -> ExecutionIntent {
    ExecutionIntent {
        root: root.to_owned(),
        identity: CommandCleanupIdentity {
            execution_id: format!("{marker_owner_pid}:{nonce}"),
            owner_pid: i64::from(owner_pid),
            pgid,
            spawned_at_ms: now_wall_ms(),
        },
        holder: holder("crashed-run"),
        spawned: pgid.is_some(),
    }
}

/// Wall-clock ms since epoch, matching what the real driver records in
/// `spawned_at_ms`. The recovery scan uses that anchor to exclude long-lived
/// same-user daemons (dumpable=0, EPERM environ — `systemd --user`,
/// `sshd`, …) from the survivor-uncertainty set; a frozen historical constant
/// would defeat the filter and every scan on a shared machine would stay
/// uncertain forever.
fn now_wall_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_millis() as u64
}

/// Spawn the controlled "host" process (a disposable `sleep`) whose pid the
/// crash will remove, plus a marked child carrying `{owner}:{nonce}` in
/// `AGENTKIT_EXEC_ID` that leads its own process group — exactly the shape
/// the driver gives a command, so a crash leaves an identifiable live group.
struct CrashFixture {
    owner_pid: i32,
    /// Pid named inside the marked child's `AGENTKIT_EXEC_ID` — the live
    /// harness, so sibling binaries' orphan sweeps spare the fixture while
    /// its recorded owner is dead (see `intent`).
    marker_owner_pid: i32,
    marked_pid: i32,
    nonce: String,
    owner: std::process::Child,
    marked: std::process::Child,
}

fn detach() -> std::process::Stdio {
    std::process::Stdio::null()
}

impl CrashFixture {
    /// Spawn owner + marked child and WAIT until the marked command has
    /// actually exec'd: a forked-but-not-exec'd child has an empty environ
    /// and no process group yet, so the marker scan would (conservatively,
    /// correctly) classify UNKNOWN — which is not the settled state the
    /// recovery assertions describe. Production tolerates the window; the
    /// test may not depend on it.
    async fn spawn() -> Self {
        let nonce = uuid::Uuid::new_v4().simple().to_string();
        let owner = std::process::Command::new("/bin/sleep")
            .arg("300")
            .stdin(detach())
            .stdout(detach())
            .stderr(detach())
            .spawn()
            .expect("spawn owner");
        let owner_pid = owner.id() as i32;
        let marker_owner_pid = std::process::id() as i32;
        let marked = marked_command()
            .env("AGENTKIT_EXEC_ID", format!("{marker_owner_pid}:{nonce}"))
            .stdin(detach())
            .stdout(detach())
            .stderr(detach())
            .process_group(0)
            .spawn()
            .expect("spawn marked child");
        let marked_pid = marked.id() as i32;
        let me = Self {
            owner_pid,
            marker_owner_pid,
            marked_pid,
            nonce,
            owner,
            marked,
        };
        let exec_id = format!("{}:{}", me.marker_owner_pid, me.nonce);
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(10);
        loop {
            if let Some(pids) =
                agentkit_toolpacks_coding::process_tree::find_marked_processes(&exec_id).await
                && pids.contains(&me.marked_pid)
            {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the marked child never presented its execution marker"
            );
            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        }
        me
    }

    /// Simulate the host crash: SIGKILL the owner (no teardown runs) and
    /// reap it, so the owner pid is gone while the marked child survives.
    fn crash_owner(&mut self) {
        unsafe { libc::kill(self.owner_pid, libc::SIGKILL) };
        let _ = self.owner.wait();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while pid_alive(self.owner_pid) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!pid_alive(self.owner_pid), "owner still alive");
        assert!(
            pid_alive(self.marked_pid),
            "the marked child must have survived the crash"
        );
    }

    fn kill_marked(&mut self) {
        unsafe { libc::kill(self.marked_pid, libc::SIGKILL) };
        let _ = self.marked.wait();
    }
}

impl Drop for CrashFixture {
    fn drop(&mut self) {
        unsafe {
            libc::kill(self.owner_pid, libc::SIGKILL);
            libc::kill(self.marked_pid, libc::SIGKILL);
        }
        let _ = self.owner.wait();
        let _ = self.marked.wait();
    }
}

async fn acquire(
    coordinator: &Arc<dyn WorkspaceCoordinator>,
    root: &str,
    run: &str,
    max_wait_ms: u64,
) -> Result<
    agentkit_toolpacks_coding::coordination::WorkspaceLease,
    agentkit_toolpacks_coding::errors::CodingError,
> {
    coordinator
        .acquire_workspace(AcquireWorkspaceInput {
            root: root.to_owned(),
            holder: holder(run),
            cancellation: None,
            max_wait_ms: Some(max_wait_ms),
            execution_id: None,
        })
        .await
}

/// Crash at the effect boundary with a LIVE process group: the reopen (a
/// fresh coordinator instance sharing nothing but the durable journal) must
/// classify the execution as unresolved cleanup — workspace blocked on both
/// the lease and the edit paths, evidence naming the exact execution nonce
/// and safe identity — and then lift the block on POSITIVE evidence (clean
/// marker scan + empty group) without operator intervention.
#[tokio::test(flavor = "multi_thread")]
async fn crashed_live_process_group_stays_uncertain_until_positive_evidence() {
    let _serial = gate().lock().await;
    let workspace = tempfile::tempdir().expect("workspace");
    let root = workspace.path().to_string_lossy().into_owned();
    let coordinator = create_in_process_workspace_coordinator(None);
    let mut crash = CrashFixture::spawn().await;
    coordinator
        .record_execution_intent(&intent(
            &root,
            crash.marker_owner_pid,
            crash.owner_pid,
            &crash.nonce,
            Some(crash.marked_pid),
        ))
        .await
        .expect("durable pre-spawn intent publishes");
    crash.crash_owner();

    // The reopen: a fresh instance shares no in-memory state with the first;
    // it must still classify the crash from the durable record before any
    // modifying work is admitted (the recovery is awaited, not forgotten).
    let reopened = create_in_process_workspace_coordinator(None);
    let error = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        acquire(&reopened, &root, "next-run", 300),
    )
    .await
    .expect("refusal is prompt")
    .expect_err("the crashed run's live group blocks modifying work");
    assert_eq!(error.code, "workspace_quarantined", "{error:?}");
    assert!(
        error.message.contains(&crash.nonce),
        "the refusal must name the exact execution: {error}"
    );

    let quarantine = reopened
        .workspace_quarantine(&root)
        .await
        .unwrap()
        .expect("unresolved cleanup is recorded");
    let identity = quarantine
        .identity
        .expect("SAFE command identity preserved");
    assert_eq!(
        identity.execution_id,
        format!("{}:{}", crash.marker_owner_pid, crash.nonce)
    );
    assert_eq!(identity.owner_pid, i64::from(crash.owner_pid));
    assert_eq!(identity.pgid, Some(crash.marked_pid));
    assert!(
        identity.spawned_at_ms >= now_wall_ms().saturating_sub(600_000)
            && identity.spawned_at_ms <= now_wall_ms() + 1_000,
        "the recorded spawn time round-trips unchanged: {}",
        identity.spawned_at_ms
    );
    assert!(quarantine.pids.contains(&crash.marked_pid));

    // The edit path is blocked by the same evidence.
    let edit = reopened
        .acquire_file(AcquireFileInput {
            root: root.clone(),
            path: format!("{root}/a.txt"),
            holder: holder("editor"),
            cancellation: None,
            max_wait_ms: Some(200),
            execution_id: None,
        })
        .await
        .expect_err("edits stay blocked while the effect is uncertain");
    assert_eq!(edit.code, "workspace_quarantined");

    // Kill the survivor: the marker scan finds nothing and the recorded
    // group is empty — positive evidence, no operator recovery needed.
    crash.kill_marked();
    assert!(wait_until_gone(crash.marked_pid, 5).await);
    let mut lease = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        acquire(&reopened, &root, "after-recovery", 2_000),
    )
    .await
    .expect("grants once the group is positively gone")
    .expect("workspace reclaimable on positive evidence");
    lease.release().await;
    assert!(
        reopened
            .workspace_quarantine(&root)
            .await
            .unwrap()
            .is_none()
    );
}

/// Crash where the declared execution is POSITIVELY gone (owner dead, no
/// marked process anywhere, recorded group empty): the reopen retires the
/// intent and admits new work — no phantom quarantine for a run that
/// demonstrably left nothing behind, and no automatic replay of anything.
#[tokio::test(flavor = "multi_thread")]
async fn crashed_execution_with_no_survivors_reopens_clean() {
    let _serial = gate().lock().await;
    let workspace = tempfile::tempdir().expect("workspace");
    let root = workspace.path().to_string_lossy().into_owned();
    let coordinator = create_in_process_workspace_coordinator(None);
    let mut crash = CrashFixture::spawn().await;
    let nonce = crash.nonce.clone();
    let owner = crash.owner_pid;
    let pgid = crash.marked_pid;
    coordinator
        .record_execution_intent(&intent(
            &root,
            crash.marker_owner_pid,
            owner,
            &nonce,
            Some(pgid),
        ))
        .await
        .expect("intent publishes");
    crash.crash_owner();
    crash.kill_marked();
    assert!(wait_until_gone(pgid, 5).await);

    let reopened = create_in_process_workspace_coordinator(None);
    let mut lease = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        acquire(&reopened, &root, "after-crash", 2_000),
    )
    .await
    .expect("prompt")
    .expect("a positively-gone execution must not quarantine the root");
    lease.release().await;
    assert!(
        reopened
            .workspace_quarantine(&root)
            .await
            .unwrap()
            .is_none()
    );
}

/// Lock-directory coordinator (the durable, cross-process kind): the intent
/// is a real file in the lock dir BEFORE any spawn, survives a full
/// coordinator teardown, and a reopened coordinator quarantines the root
/// with the intent's safe identity. Only explicit recovery releases it.
#[tokio::test(flavor = "multi_thread")]
async fn lockdir_intent_survives_reopen_and_blocks_until_explicit_recovery() {
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

    let coordinator = create_lockdir_workspace_coordinator(options());
    let mut crash = CrashFixture::spawn().await;
    let nonce = crash.nonce.clone();
    coordinator
        .record_execution_intent(&intent(
            &root,
            crash.marker_owner_pid,
            crash.owner_pid,
            &nonce,
            Some(crash.marked_pid),
        ))
        .await
        .expect("intent publishes");

    // The intent is on disk in the lock dir BEFORE the crash — a durable
    // pre-spawn ownership record, not memory.
    let intent_files = std::fs::read_dir(&dir)
        .expect("lock dir")
        .flatten()
        .filter_map(|entry| {
            let file_type = entry.file_type().ok()?;
            file_type.is_dir().then(|| {
                std::fs::read_dir(entry.path())
                    .ok()
                    .into_iter()
                    .flatten()
                    .flatten()
                    .filter(|inner| {
                        let name = inner.file_name().to_string_lossy().into_owned();
                        name.starts_with("intent-") && name.ends_with(".json")
                    })
                    .count()
            })
        })
        .sum::<usize>();
    assert!(intent_files >= 1, "durable intent file must exist on disk");

    crash.crash_owner();
    drop(coordinator); // full teardown of the crashed process's coordinator

    // Reopen: a brand-new coordinator over the same durable directory.
    let reopened = create_lockdir_workspace_coordinator(options());
    let error = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        acquire(&reopened, &root, "next-run", 300),
    )
    .await
    .expect("prompt")
    .expect_err("the crashed run must block the reopened host");
    assert_eq!(error.code, "workspace_quarantined", "{error:?}");
    assert!(error.message.contains(&nonce));
    let quarantine = reopened
        .workspace_quarantine(&root)
        .await
        .unwrap()
        .expect("durable quarantine");
    assert_eq!(
        quarantine
            .identity
            .as_ref()
            .map(|identity| identity.execution_id.clone()),
        Some(format!("{}:{}", crash.marker_owner_pid, nonce))
    );

    // The survivor is still alive: an operator's explicit recovery lifts the
    // hold (the supported escape hatch), and modifying work resumes.
    assert!(pid_alive(crash.marked_pid));
    assert!(
        reopened
            .clear_workspace_quarantine(&root)
            .await
            .expect("recovery call")
    );
    let mut lease = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        acquire(&reopened, &root, "recovered-run", 2_000),
    )
    .await
    .expect("prompt")
    .expect("explicit recovery admits the next run");
    lease.release().await;
    crash.kill_marked();
}

/// Unknown cleanup never self-clears: an intent that died before its
/// process group was recorded (pgid `None`) cannot be positively proven
/// gone, so the root stays blocked even after the marked process is killed,
/// until recovery is explicitly recorded.
#[tokio::test(flavor = "multi_thread")]
async fn unknown_group_keeps_the_root_blocked_until_explicit_recovery() {
    let _serial = gate().lock().await;
    let workspace = tempfile::tempdir().expect("workspace");
    let root = workspace.path().to_string_lossy().into_owned();
    let coordinator = create_in_process_workspace_coordinator(None);
    let mut crash = CrashFixture::spawn().await;
    let nonce = crash.nonce.clone();
    coordinator
        .record_execution_intent(&intent(
            &root,
            crash.marker_owner_pid,
            crash.owner_pid,
            &nonce,
            None,
        ))
        .await
        .expect("intent publishes");
    crash.crash_owner();

    let reopened = create_in_process_workspace_coordinator(None);
    let error = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        acquire(&reopened, &root, "next-run", 300),
    )
    .await
    .expect("prompt")
    .expect_err("unknown cleanup is blocking evidence");
    assert_eq!(error.code, "workspace_quarantined");

    // Even after the marked child is dead, the group was never recorded —
    // cleanup cannot be positively established, so the hold persists.
    crash.kill_marked();
    assert!(wait_until_gone(crash.marked_pid, 5).await);
    let error = acquire(&reopened, &root, "next-run", 300)
        .await
        .expect_err("unknown must not silently resolve");
    assert_eq!(error.code, "workspace_quarantined");
    assert!(error.message.contains(&nonce));

    assert!(
        reopened
            .clear_workspace_quarantine(&root)
            .await
            .expect("recovery")
    );
    let mut lease = acquire(&reopened, &root, "recovered", 2_000)
        .await
        .expect("explicit recovery unblocks");
    lease.release().await;
}

/// A corrupt (unreadable) intent record is UNKNOWN cleanup, not a success:
/// the reopen must block the root and surface explicit evidence, and only
/// recorded recovery releases it. This is the B5 rule that failed quarantine
/// / unknown-cleanup persistence is BLOCKING evidence, never swallowed.
#[tokio::test(flavor = "multi_thread")]
async fn corrupt_intent_blocks_instead_of_silently_retiring() {
    let _serial = gate().lock().await;
    let workspace = tempfile::tempdir().expect("workspace");
    let root = workspace.path().to_string_lossy().into_owned();
    let coordinator = create_in_process_workspace_coordinator(None);
    let mut crash = CrashFixture::spawn().await;
    let nonce = crash.nonce.clone();
    coordinator
        .record_execution_intent(&intent(
            &root,
            crash.marker_owner_pid,
            crash.owner_pid,
            &nonce,
            None,
        ))
        .await
        .expect("intent publishes");
    crash.crash_owner();

    // Corrupt the durable record the way a torn write would.
    let journal = std::env::temp_dir().join("agentkit-exec-intents");
    let mut corrupted = false;
    if let Some(entries) = walk(&journal) {
        for path in entries {
            if !path.extension().is_some_and(|ext| ext == "json") {
                continue;
            }
            if let Ok(text) = std::fs::read_to_string(&path)
                && text.contains(&nonce)
            {
                std::fs::write(&path, b"{ truncated").expect("corrupt intent");
                corrupted = true;
                break;
            }
        }
    }
    assert!(corrupted, "the durable intent file must exist to corrupt");

    let reopened = create_in_process_workspace_coordinator(None);
    let error = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        acquire(&reopened, &root, "next-run", 300),
    )
    .await
    .expect("prompt")
    .expect_err("a corrupt record is blocking evidence");
    assert_eq!(error.code, "workspace_quarantined", "{error:?}");
    assert!(
        reopened
            .clear_workspace_quarantine(&root)
            .await
            .expect("recovery")
    );
    let mut lease = acquire(&reopened, &root, "recovered", 2_000)
        .await
        .expect("recovery after a corrupt record releases the root");
    lease.release().await;
}

fn walk(root: &std::path::Path) -> Option<Vec<std::path::PathBuf>> {
    let mut stack = vec![root.to_path_buf()];
    let mut files = Vec::new();
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
    (!files.is_empty()).then_some(files)
}

/// B5 item 2: startup cleanup must be AWAITED before modifying work is
/// accepted — not spawned and forgotten. This test re-executes itself as a
/// FRESH process (so the process-once startup recovery is genuinely
/// pending), spawns an orphan whose owner pid is dead, and asserts the very
/// first `acquire_workspace` in that process returns only after the orphan
/// has actually been terminated — the lease grant is gated on recovery.
#[tokio::test(flavor = "multi_thread")]
async fn startup_cleanup_is_awaited_before_the_first_lease() {
    if let Some(dir) = std::env::var_os("AGENTKIT_GATE_CHILD") {
        gate_child(std::path::PathBuf::from(dir)).await;
        return;
    }
    let _serial = gate().lock().await;
    let workspace = tempfile::tempdir().expect("workspace");
    let output = std::process::Command::new(std::env::current_exe().expect("exe"))
        .arg("startup_cleanup_is_awaited_before_the_first_lease")
        .arg("--exact")
        .arg("--nocapture")
        .env("AGENTKIT_GATE_CHILD", workspace.path())
        .output()
        .expect("spawn gate child");
    assert!(
        output.status.success(),
        "gate child failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

async fn gate_child(workspace: std::path::PathBuf) {
    let root = workspace.to_string_lossy().into_owned();
    let nonce = uuid::Uuid::new_v4().simple().to_string();

    // An orphan: marked with an owner pid that is already dead, exactly what
    // a previous crashed host leaves behind before startup recovery runs.
    let mut owner = std::process::Command::new("/bin/sleep")
        .arg("300")
        .stdin(detach())
        .stdout(detach())
        .stderr(detach())
        .spawn()
        .expect("spawn owner");
    let owner_pid = owner.id() as i32;
    unsafe { libc::kill(owner_pid, libc::SIGKILL) };
    let _ = owner.wait();
    let orphan = marked_command()
        .env("AGENTKIT_EXEC_ID", format!("{owner_pid}:{nonce}"))
        .stdin(detach())
        .stdout(detach())
        .stderr(detach())
        .process_group(0)
        .spawn()
        .expect("spawn orphan");
    let mut orphan = ChildGuard(orphan);
    let orphan_pid = orphan.0.id() as i32;
    assert!(
        pid_alive(orphan_pid),
        "the orphan must be alive pre-recovery"
    );
    let exec_id = format!("{owner_pid}:{nonce}");
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(10);
    loop {
        if agentkit_toolpacks_coding::process_tree::find_marked_processes(&exec_id)
            .await
            .is_some_and(|pids| pids.contains(&orphan_pid))
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "orphan marker not observable"
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    }

    let coordinator = create_in_process_workspace_coordinator(None);
    let grant = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        acquire(&coordinator, &root, "first-work", 5_000),
    )
    .await
    .expect("acquisition completes")
    .expect("the first modifying lease is granted after recovery");

    // The grant returned — the orphan must already be dead: startup recovery
    // completed inside the acquisition, proving it is awaited/gating rather
    // than spawned-and-forgotten.
    let killed = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Some(status) = orphan.0.try_wait().expect("reap orphan") {
                break status;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the startup sweep must have terminated the orphan before/at grant");
    assert!(killed.code().is_none(), "orphan died by signal: {killed:?}");
    assert!(!pid_alive(orphan_pid));
    drop(grant);
}

struct ChildGuard(std::process::Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
