//! Defect B2 across real processes: a writer that is QUEUED behind the
//! lock-directory marker must not be handed an executable lease when a
//! quarantine appears while it waits.
//!
//! The in-process gates cannot show this (a queued waiter in one process is
//! stopped by its own in-process gate long before the durable record
//! matters), so the queued writer runs as a second OS process: the test binary
//! is re-executed with an environment marker and runs the child case below.
//!
//! Deterministic ordering, no correctness-bearing sleeps:
//!  1. the parent holds the workspace marker;
//!  2. the child starts acquiring and reports through the coordinator observer
//!     that it is polling for that marker (an atomic flag file);
//!  3. only then does the parent publish the quarantine (naming a live child
//!     process it started) and release its lease;
//!  4. the child must come back `QUARANTINED`, never `ACQUIRED`.
//!
//! Every subprocess is reaped by a Drop guard even when an assertion fails,
//! and every wait is bounded so a regression fails instead of hanging.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use agentkit_toolpacks_coding::coordination::{
    AcquireWorkspaceInput, CoordinationWait, CoordinatorObserver, LeaseHolder, QuarantineEvidence,
    WaitKind,
};
use agentkit_toolpacks_coding::coordination_lockdir::{
    LockDirCoordinatorOptions, create_lockdir_workspace_coordinator,
};

/// Set in the re-executed test binary: it then runs the queued-writer case
/// instead of the suite.
const CHILD_ENV: &str = "AGENTKIT_QUARANTINE_RACE_CHILD";
const DIR_ENV: &str = "AGENTKIT_QUARANTINE_RACE_DIR";
const ROOT_ENV: &str = "AGENTKIT_QUARANTINE_RACE_ROOT";
const FLAG_ENV: &str = "AGENTKIT_QUARANTINE_RACE_FLAG";
const OUTCOME_ENV: &str = "AGENTKIT_QUARANTINE_RACE_OUTCOME";

const ACQUIRED: &str = "ACQUIRED";
const QUARANTINED: &str = "QUARANTINED";

fn holder(run: &str, tool: &str, label: &str) -> LeaseHolder {
    LeaseHolder {
        chat_id: Some("chat-race".to_owned()),
        run_id: run.to_owned(),
        tool_call_id: Some(format!("call-{run}")),
        tool: tool.to_owned(),
        label: label.to_owned(),
    }
}

/// Removes the disposable workspace on every exit path, including a panicking
/// assertion.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "ak-quarantine-race-{}-{name}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&path).expect("create scratch dir");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Kills and reaps a subprocess on every exit path: a leaked `sleep` would
/// leave a quarantine survivor alive for the rest of the suite, and a leaked
/// writer would keep a marker file behind.
struct Reap(Option<std::process::Child>);

impl Reap {
    fn spawn(builder: &mut std::process::Command) -> Self {
        Self(Some(
            builder
                .stdin(std::process::Stdio::null())
                .spawn()
                .expect("spawn subprocess"),
        ))
    }

    fn child(&mut self) -> &mut std::process::Child {
        self.0.as_mut().expect("child not yet taken")
    }

    /// Hand the child over once it has been observed to exit; the guard then
    /// has nothing left to reap.
    fn take(&mut self) -> Option<std::process::Child> {
        self.0.take()
    }
}

impl Drop for Reap {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Writes the "I am queued" flag from `on_waiting`, atomically so the parent
/// never observes a half-written file.
struct FlagObserver {
    flag: PathBuf,
}

impl CoordinatorObserver for FlagObserver {
    fn on_waiting(&self, wait: &CoordinationWait) {
        if wait.kind != WaitKind::Workspace {
            return;
        }
        let temp = self.flag.with_extension("tmp");
        if std::fs::write(&temp, format!("waiting:{}", wait.waiter.run_id)).is_ok() {
            let _ = std::fs::rename(&temp, &self.flag);
        }
    }
}

fn write_outcome(marker: &str) {
    let path = std::env::var(OUTCOME_ENV).expect("outcome path");
    let _ = std::fs::write(path, marker);
}

/// The queued-writer side, run in the re-executed test binary. Returns the
/// process exit code: 0 only for the correct `workspace_quarantined` refusal.
async fn child_body() -> i32 {
    let dir = std::env::var(DIR_ENV).expect("lock dir");
    let root = std::env::var(ROOT_ENV).expect("workspace root");
    let flag = std::env::var(FLAG_ENV).expect("queued flag path");
    let observer = Arc::new(FlagObserver {
        flag: PathBuf::from(flag),
    });
    let coordinator = create_lockdir_workspace_coordinator(LockDirCoordinatorOptions {
        dir,
        observer: Some(observer),
        poll_ms: Some(20),
        on_error: None,
    });
    // Bounded so a parent that dies cannot leave the child spinning: a wait
    // cap expiry surfaces as `workspace_busy`, which the parent reports as a
    // failed regression rather than a hang.
    let outcome = coordinator
        .acquire_workspace(AcquireWorkspaceInput {
            root,
            holder: holder("child", "run_command", "make test"),
            cancellation: None,
            max_wait_ms: Some(20_000),
            execution_id: Some("exec-child".to_owned()),
        })
        .await;
    match outcome {
        Ok(mut lease) => {
            // The defect: a live quarantine was published while this writer
            // queued, and it still got an executable lease.
            write_outcome(ACQUIRED);
            lease.release().await;
            3
        }
        Err(error) if error.code == "workspace_quarantined" => {
            write_outcome(QUARANTINED);
            0
        }
        Err(error) => {
            write_outcome(&format!("OTHER:{}", error.code));
            4
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn queued_writer_learns_the_workspace_is_quarantined() {
    if std::env::var_os(CHILD_ENV).is_some() {
        // This process exists to serve as the queued writer; see the case
        // below. The parent drives it.
        return;
    }
    let scratch = Scratch::new("race");
    let lock_dir = scratch.path().join("locks");
    let root = scratch.path().join("ws");
    std::fs::create_dir_all(&root).expect("create workspace");
    let flag = scratch.path().join("queued.flag");
    let outcome_path = scratch.path().join("child.outcome");

    // The survivor the quarantine names: a live process that must keep the
    // workspace blocked. Reaped by the guard even if an assertion fails.
    let mut survivor = Reap::spawn(std::process::Command::new("sleep").arg("60"));
    let survivor_pid = survivor.child().id() as i32;
    assert!(survivor_pid > 0, "survivor pid");

    let coordinator = create_lockdir_workspace_coordinator(LockDirCoordinatorOptions {
        dir: lock_dir.to_string_lossy().into_owned(),
        observer: None,
        poll_ms: Some(20),
        on_error: None,
    });
    let mut held = coordinator
        .acquire_workspace(AcquireWorkspaceInput {
            root: root.to_string_lossy().into_owned(),
            holder: holder("parent", "run_command", "make install"),
            cancellation: None,
            max_wait_ms: Some(10_000),
            execution_id: Some("exec-parent".to_owned()),
        })
        .await
        .expect("the parent takes the workspace marker");

    let exe = std::env::current_exe().expect("test binary");
    let mut command = std::process::Command::new(exe);
    command
        .arg("child_writer_is_the_queued_writer")
        .arg("--exact")
        .arg("--nocapture")
        .env(CHILD_ENV, "1")
        .env(DIR_ENV, &lock_dir)
        .env(ROOT_ENV, &root)
        .env(FLAG_ENV, &flag)
        .env(OUTCOME_ENV, &outcome_path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let mut writer = Reap::spawn(&mut command);

    // 2. The child is queued only once its observer has fired.
    wait_for("the writer to queue behind the marker", || {
        flag.exists().then_some(())
    })
    .await;

    // 3. Publish the blocking evidence while the parent still owns the lease
    //    (the holds-quarantine-while-publishing invariant), then let go.
    coordinator
        .quarantine_workspace(
            &root.to_string_lossy(),
            &QuarantineEvidence {
                pids: vec![survivor_pid],
                holder: holder("parent", "run_command", "make install"),
            },
        )
        .await
        .expect("blocking evidence publishes");
    held.release().await;

    // 4. The child must refuse, not acquire.
    let status = wait_for("the writer to resolve", || {
        writer.child().try_wait().ok().flatten()
    })
    .await;
    let output = writer
        .take()
        .expect("writer exited")
        .wait_with_output()
        .expect("read writer output");
    let marker = std::fs::read_to_string(&outcome_path)
        .unwrap_or_else(|error| panic!("the writer left no outcome: {error}"));
    assert_eq!(
        marker.trim(),
        QUARANTINED,
        "a queued writer was admitted over a live quarantine (exit {status:?})"
    );
    assert_eq!(status.code(), Some(0), "correct refusal exit code");
    assert_eq!(
        output.status.code(),
        Some(0),
        "the writer exits 0 on a correct refusal"
    );

    // Positive cleanup evidence: once the survivor is gone the root is
    // workable again, so the refusal above was evidence-driven, not a wedge.
    let mut survivor = survivor.take().expect("survivor still ours to reap");
    let _ = survivor.kill();
    survivor.wait().expect("reap survivor");
    // Reaped, not a zombie: a leftover live pid would keep every later
    // quarantine in this process "alive".
    // SAFETY: kill(pid, 0) performs error checking only, sends no signal.
    assert_ne!(
        unsafe { libc::kill(survivor_pid, 0) },
        0,
        "survivor {survivor_pid} is still signalable after kill+wait"
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH),
        "survivor {survivor_pid} was not reaped"
    );
    let mut reclaimed = tokio::time::timeout(
        Duration::from_secs(20),
        coordinator.acquire_workspace(AcquireWorkspaceInput {
            root: root.to_string_lossy().into_owned(),
            holder: holder("after", "run_command", "make test"),
            cancellation: None,
            max_wait_ms: Some(10_000),
            execution_id: Some("exec-after".to_owned()),
        }),
    )
    .await
    .expect("the workspace becomes usable once cleanup is positively established")
    .expect("acquires");
    reclaimed.release().await;
}

/// The queued-writer side of the race. Only meaningful when the case above
/// re-executes this binary with [`CHILD_ENV`]; under a normal run it is inert.
#[tokio::test(flavor = "multi_thread")]
async fn child_writer_is_the_queued_writer() {
    if std::env::var_os(CHILD_ENV).is_none() {
        return;
    }
    let code = child_body().await;
    // Exit directly: the harness would otherwise report the child's own pass
    // and buffer its stdout behind a pipe.
    std::process::exit(code);
}

/// Poll a shared condition until it holds, failing rather than hanging. The
/// condition is produced by the other actor (an observer flag, a process
/// exit), never by a sleep.
async fn wait_for<T>(what: &str, mut ready: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(value) = ready() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
