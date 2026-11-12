mod support;
use agentkit_store::*;
use serde_json::json;
use std::{
    process::{Command, Stdio},
    time::{Duration, Instant},
};
use support::*;

#[tokio::test]
async fn crash_process_worker() {
    let Ok(path) = std::env::var("AGENTKIT_STORE_CRASH_PATH") else {
        return;
    };
    let store = SqliteStore::open(path, 2).unwrap();
    store.submit(task("crashed", "s")).await.unwrap();
    let lease = store.claim(claim(1000)).await.unwrap().unwrap().lease;
    store
        .append_events(lease.fence(), vec![event(0, "before-crash")], 1001)
        .await
        .unwrap();
    std::fs::write(
        std::env::var("AGENTKIT_STORE_READY_PATH").unwrap(),
        b"ready",
    )
    .unwrap();
    loop {
        std::thread::park();
    }
}

#[tokio::test]
async fn killed_process_reopens_with_durable_recovery_before_new_claim() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("agent.db");
    let ready = directory.path().join("ready");
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "crash_process_worker", "--nocapture"])
        .env("AGENTKIT_STORE_CRASH_PATH", &path)
        .env("AGENTKIT_STORE_READY_PATH", &ready)
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    wait_for_file(&ready, &mut child, "crash worker readiness");
    child.kill().unwrap();
    child.wait().unwrap();
    let store = SqliteStore::open(&path, 2).unwrap();
    assert_eq!(
        store.get_task("crashed").await.unwrap().unwrap().status,
        TaskStatus::Running
    );
    assert_eq!(
        store.events("crashed", None, 10).await.unwrap()[0].event,
        event(0, "before-crash")
    );
    let recovered = store
        .recover(1100, 3, 50, RecoveryMode::Automatic)
        .await
        .unwrap();
    assert_eq!(recovered[0].available_at, 1150);
    assert!(store.claim(claim(1149)).await.unwrap().is_none());
    drop(store);
    let reopened = SqliteStore::open(&path, 2).unwrap();
    let task = reopened.get_task("crashed").await.unwrap().unwrap();
    assert_eq!(task.status, TaskStatus::Queued);
    assert_eq!(task.poison_count, 1);
    assert_eq!(
        reopened.attempts("crashed").await.unwrap()[0].status,
        "abandoned"
    );
    assert!(
        reopened
            .outbox()
            .await
            .unwrap()
            .iter()
            .any(|record| record.payload["type"] == "task.recovered")
    );
    assert_eq!(
        reopened
            .claim(claim(1150))
            .await
            .unwrap()
            .unwrap()
            .task
            .attempt_count,
        2
    );
}

#[tokio::test]
async fn ownership_is_exclusive_and_released_on_last_clone_drop() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("agent.db");
    let store = SqliteStore::open(&path, 2).unwrap();
    let clone = store.clone();
    #[cfg(unix)]
    {
        let alias = directory.path().join("hard-link.db");
        std::fs::hard_link(&path, &alias).unwrap();
        assert!(matches!(
            SqliteStore::open(&alias, 2),
            Err(StoreError::AlreadyOwned)
        ));
    }
    assert!(matches!(
        SqliteStore::open(&path, 2),
        Err(StoreError::AlreadyOwned)
    ));
    drop(store);
    assert!(matches!(
        SqliteStore::open(&path, 2),
        Err(StoreError::AlreadyOwned)
    ));
    clone
        .set_setting("test", "saved", json!({"a":1}))
        .await
        .unwrap();
    drop(clone);
    let reopened = SqliteStore::open(&path, 2).unwrap();
    assert_eq!(
        reopened.get_setting("test", "saved").await.unwrap(),
        Some(json!({"a":1}))
    );
}

#[test]
fn foreign_and_newer_databases_are_refused_without_database_writes() {
    let directory = tempfile::tempdir().unwrap();
    let foreign = directory.path().join("typescript.db");
    {
        let connection = rusqlite::Connection::open(&foreign).unwrap();
        connection
            .execute_batch("CREATE TABLE chats(id TEXT);PRAGMA user_version=8;")
            .unwrap();
    }
    let before = std::fs::read(&foreign).unwrap();
    assert!(matches!(
        SqliteStore::open(&foreign, 2),
        Err(StoreError::ForeignDatabase)
    ));
    assert_eq!(std::fs::read(&foreign).unwrap(), before);
    let newer = directory.path().join("newer.db");
    drop(SqliteStore::open(&newer, 2).unwrap());
    {
        let connection = rusqlite::Connection::open(&newer).unwrap();
        connection
            .pragma_update(None, "user_version", agentkit_store::SCHEMA_VERSION + 1)
            .unwrap();
    }
    let before = std::fs::read(&newer).unwrap();
    assert!(matches!(
        SqliteStore::open(&newer, 2),
        Err(StoreError::UnsupportedVersion(v)) if v == agentkit_store::SCHEMA_VERSION + 1
    ));
    assert_eq!(std::fs::read(&newer).unwrap(), before);
}

#[test]
fn malformed_same_version_schema_is_refused_before_persistent_pragmas() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("malformed.db");
    drop(SqliteStore::open(&path, 2).unwrap());
    {
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .execute_batch("PRAGMA journal_mode=DELETE; DROP INDEX proposal_action;")
            .unwrap();
    }
    let before = std::fs::read(&path).unwrap();
    assert!(matches!(
        SqliteStore::open(&path, 2),
        Err(StoreError::ForeignDatabase)
    ));
    assert_eq!(std::fs::read(&path).unwrap(), before);
    let connection = rusqlite::Connection::open(&path).unwrap();
    let mode: String = connection
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .unwrap();
    assert_eq!(mode, "delete");
}

#[tokio::test]
async fn reopened_handle_enforces_persisted_fence_without_partial_writes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("fence.db");
    let store = SqliteStore::open(&path, 4).unwrap();
    store.submit(task("run", "scope")).await.unwrap();
    let lease = store.claim(claim(1000)).await.unwrap().unwrap().lease;
    drop(store);
    let reopened = SqliteStore::open(&path, 4).unwrap();
    // A second live handle over the same file is refused outright.
    assert!(matches!(
        SqliteStore::open(&path, 4),
        Err(StoreError::AlreadyOwned)
    ));
    // Lease authority lives in the database: the same fence keeps working
    // on a fresh handle, and expiry is enforced there too.
    reopened.verify_fence(&lease.fence(), 1001).await.unwrap();
    reopened
        .append_events(lease.fence(), vec![event(0, "after-reopen")], 1001)
        .await
        .unwrap();
    assert!(matches!(
        reopened
            .append_events(lease.fence(), vec![event(1, "expired")], 1100)
            .await,
        Err(StoreError::LeaseLost(id)) if id == "run"
    ));
    // New authority minted by the fresh handle fences the old writer out.
    let replacement = reopened
        .acquire_lease("run", &lease.attempt_id, "worker-b", 1150, 100)
        .await
        .unwrap();
    assert!(replacement.fencing_token > lease.fencing_token);
    assert!(matches!(
        reopened
            .append_events(lease.fence(), vec![event(1, "zombie")], 1160)
            .await,
        Err(StoreError::LeaseLost(id)) if id == "run"
    ));
    assert!(matches!(
        reopened
            .settle(SettleTask {
                fence: lease.fence(),
                now: 1160,
                status: TaskStatus::Completed,
                error: None,
                events: vec![event(1, "zombie-settle")],
                messages: vec![],
            })
            .await,
        Err(StoreError::LeaseLost(id)) if id == "run"
    ));
    // Zero partial writes: only the committed event survives, no zombie
    // payload reached the outbox, and task/attempt state is untouched.
    let events = reopened.events("run", None, 10).await.unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event, event(0, "after-reopen"));
    assert!(
        !reopened
            .outbox()
            .await
            .unwrap()
            .iter()
            .any(|record| record.payload.to_string().contains("zombie"))
    );
    let attempts = reopened.attempts("run").await.unwrap();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].status, "running");
    assert_eq!(
        reopened.get_task("run").await.unwrap().unwrap().status,
        TaskStatus::Running
    );
    // The refused batches advanced no sequence watermark: the fresh fence
    // still owns the run and continues exactly at seq 1.
    reopened
        .append_events(
            replacement.fence(),
            vec![event(1, "after-replacement")],
            1160,
        )
        .await
        .unwrap();
    finish(&reopened, replacement, TaskStatus::Completed, 1170).await;
    drop(reopened);
    let final_handle = SqliteStore::open(&path, 4).unwrap();
    let events = final_handle.events("run", None, 10).await.unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[1].event, event(1, "after-replacement"));
}

#[tokio::test]
async fn invalid_envelope_after_valid_prefix_fails_typed_and_commits_nothing() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("rollback.db");
    let store = SqliteStore::open(&path, 4).unwrap();
    store.submit(task("run", "scope")).await.unwrap();
    let lease = store.claim(claim(1000)).await.unwrap().unwrap().lease;
    let mut invalid = event(1, "invalid");
    invalid["eventId"] = json!(1);
    let error = store
        .append_events(lease.fence(), vec![event(0, "prefix"), invalid], 1001)
        .await
        .unwrap_err();
    assert!(
        matches!(&error, StoreError::InvalidInput(message)
            if message.starts_with("invalid task event envelope:")),
        "{error}"
    );
    // The valid first event of the batch committed nothing anywhere.
    assert!(store.events("run", None, 10).await.unwrap().is_empty());
    assert!(
        !store
            .outbox()
            .await
            .unwrap()
            .iter()
            .any(|record| record.payload["eventId"] == json!("prefix"))
    );
    // No watermark leaked from the rolled-back prefix: a fully valid batch
    // starting again at seq 0 still lands.
    store
        .append_events(lease.fence(), vec![event(0, "retried")], 1001)
        .await
        .unwrap();
    let events = store.events("run", None, 10).await.unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event, event(0, "retried"));
    drop(store);
    let reopened = SqliteStore::open(&path, 4).unwrap();
    let events = reopened.events("run", None, 10).await.unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event, event(0, "retried"));
}

// ---------------------------------------------------------------------------
// Injected-fault durability cells (AGENTKIT-349).
//
// The fault plan arrives through `AGENTKIT_STORE_FAULT`, read once per store
// open by `store/src/faults.rs` (production-inert: unset environment, no
// behavior). Children follow the `crash_process_worker` convention: one
// `#[tokio::test]` per scenario that early-returns unless its private path
// variable is set, driven by a parent test through `--exact`.
// ---------------------------------------------------------------------------

fn spawn_fault_child(
    name: &str,
    dir: &std::path::Path,
    extra_env: &[(&str, &str)],
) -> std::process::Child {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", name, "--nocapture"])
        .env("AGENTKIT_STORE_DB_PATH", dir.join("agent.db"))
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (key, value) in extra_env {
        command.env(key, value);
    }
    command.spawn().unwrap()
}

/// Shared child-readiness wait for this file. Generous on purpose: the
/// child re-executes this whole test binary, and on a loaded machine (CI
/// with parallel builds) even spawning it can take tens of seconds; a short
/// deadline here is a harness lie, not a durability signal. Returns when the
/// file appears; panics if the child dies first or the budget runs out.
fn wait_for_file(path: &std::path::Path, child: &mut std::process::Child, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(90);
    while !path.exists() {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("{what} did not arrive: child exited {status:?}");
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("{what} did not arrive within the budget");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn batch(token: &str, first_seq: u64, count: u64) -> Vec<serde_json::Value> {
    (0..count)
        .map(|index| event(first_seq + index, &format!("{token}-{index}")))
        .collect()
}

/// Read a file the way a storage inspector would.
///
/// macOS: `F_NOCACHE` (fcntl 48) serves the read around the unified buffer
/// cache. Linux/other unix: there is NO per-read cache-bypass primitive —
/// `F_NOCACHE` does not exist there (fcntl 48 is unassigned → EINVAL, which
/// is why this leg used to fail every Linux run) and `O_DIRECT` would lose
/// the sub-sector WAL tail. The honest Linux assertion is therefore the
/// WAL image as the kernel presents it after the writer died: wholly
/// serialized, wholly readable by a writer-oblivious reader, never torn.
/// Whether the fsync itself fired is falsified by the `faults` pragma unit
/// tests, not by user-space reads (see the platform note on the test).
#[cfg(unix)]
fn read_bypassing_cache(path: &std::path::Path) -> std::io::Result<Vec<u8>> {
    #[cfg(target_os = "macos")]
    {
        use std::ffi::c_int;
        use std::os::fd::AsRawFd;
        unsafe extern "C" {
            fn fcntl(fd: c_int, cmd: c_int, arg: c_int) -> c_int;
        }
        const F_NOCACHE: c_int = 48;
        let mut file = std::fs::File::open(path)?;
        if unsafe { fcntl(file.as_raw_fd(), F_NOCACHE, 1) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut file, &mut bytes)?;
        Ok(bytes)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let mut file = std::fs::File::open(path)?;
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut file, &mut bytes)?;
        Ok(bytes)
    }
}

/// Child body: pre-fault commits as positive control, then one injected
/// root-commit refusal and one injected ambient-commit refusal, then both
/// paths must succeed again once the refusal budget is spent.
#[tokio::test]
async fn fault_child_write_refusal() {
    let Ok(path) = std::env::var("AGENTKIT_STORE_DB_PATH") else {
        return;
    };
    let outcome_path = std::env::var("AGENTKIT_STORE_OUTCOME_PATH").unwrap();
    let store = SqliteStore::open(path, 2).unwrap();
    let mut outcomes = Vec::new();
    let record = |outcomes: &mut Vec<String>, step: &str, result: &dyn std::fmt::Debug| {
        outcomes.push(format!("{step}={result:?}"));
    };
    // Positive control: the same code path commits truthfully before and
    // after the fault window, so a broken harness can never fake a refusal.
    store.submit(task("run", "scope")).await.unwrap();
    let lease = store.claim(claim(1000)).await.unwrap().unwrap().lease;
    store
        .append_events(lease.fence(), vec![event(0, "pre-fault")], 1001)
        .await
        .unwrap();
    record(&mut outcomes, "control", &"ok");
    let error = store
        .append_events(lease.fence(), vec![event(1, "refused-root")], 1001)
        .await
        .unwrap_err();
    record(&mut outcomes, "refused-root", &error.to_string());
    let transaction = store.begin_transaction().await.unwrap();
    transaction
        .view()
        .set_setting("fault", "ambient", json!({"a": 1}))
        .await
        .unwrap();
    let error = transaction.commit().await.unwrap_err();
    record(&mut outcomes, "refused-ambient-commit", &error.to_string());
    // The refusal budget is spent: the refused watermark is untouched, so a
    // batch starting again at seq 1 lands, and the ambient path works too.
    store
        .append_events(lease.fence(), vec![event(1, "recovered")], 1001)
        .await
        .unwrap();
    record(&mut outcomes, "recovered-root", &"ok");
    let transaction = store.begin_transaction().await.unwrap();
    transaction
        .view()
        .set_setting("pre", "retry", json!({"b": 2}))
        .await
        .unwrap();
    transaction.commit().await.unwrap();
    record(&mut outcomes, "recovered-ambient", &"ok");
    store.flush().await.unwrap();
    std::fs::write(outcome_path, outcomes.join("\n")).unwrap();
    drop(store);
}

#[tokio::test]
async fn injected_commit_refusal_commits_nothing_and_recovers_after_clear() {
    let directory = tempfile::tempdir().unwrap();
    let outcomes = directory.path().join("outcomes");
    let mut child = spawn_fault_child(
        "fault_child_write_refusal",
        directory.path(),
        &[
            // submit, claim and the pre-fault append commit first (3), then
            // the root and ambient durability points each refuse once (2).
            ("AGENTKIT_STORE_FAULT", "refuse=3:2"),
            ("AGENTKIT_STORE_OUTCOME_PATH", outcomes.to_str().unwrap()),
        ],
    );
    let status = child.wait().unwrap();
    assert!(status.success(), "refusal child exited {status:?}");
    let transcript = std::fs::read_to_string(&outcomes).unwrap();
    // Truthful refusal on both durability choke points, shaped like a real
    // storage error; and the control plus both retries are clean successes.
    for step in [
        "control=\"ok\"",
        "recovered-root=\"ok\"",
        "recovered-ambient=\"ok\"",
    ] {
        assert!(transcript.contains(step), "{transcript}");
    }
    for step in ["refused-root=", "refused-ambient-commit="] {
        let line = transcript
            .lines()
            .find(|line| line.starts_with(step))
            .unwrap_or_else(|| panic!("{step} missing in {transcript}"));
        assert!(
            line.contains("injected write refusal"),
            "refusal must be truthful: {line}"
        );
    }
    // Pre-fault rows intact, refused batches committed nothing anywhere —
    // no events, no outbox residue, no settings residue, no watermark leak.
    let store = SqliteStore::open(directory.path().join("agent.db"), 2).unwrap();
    let events = store.events("run", None, 10).await.unwrap();
    assert_eq!(events.len(), 2, "pre-fault + recovered only");
    assert_eq!(events[0].event, event(0, "pre-fault"));
    assert_eq!(events[1].event, event(1, "recovered"));
    let payloads: Vec<String> = store
        .outbox()
        .await
        .unwrap()
        .iter()
        .map(|record| record.payload.to_string())
        .collect();
    assert!(
        !payloads
            .iter()
            .any(|payload| payload.contains("refused-root")),
        "refused batch left outbox residue: {payloads:?}"
    );
    assert_eq!(store.get_setting("fault", "ambient").await.unwrap(), None);
    assert_eq!(
        store.get_setting("pre", "retry").await.unwrap(),
        Some(json!({"b": 2}))
    );
    drop(store);
    // Reopen is healthy: the refused commits never reached the file.
    let reopened = SqliteStore::open(directory.path().join("agent.db"), 2).unwrap();
    assert_eq!(reopened.events("run", None, 10).await.unwrap().len(), 2);
}

/// Child body: commits an acknowledged batch under the production
/// durability settings, then parks; the parent kills it and inspects
/// storage directly.
#[tokio::test]
async fn durable_child_acknowledged_batch() {
    let Ok(path) = std::env::var("AGENTKIT_STORE_DB_PATH") else {
        return;
    };
    let token = std::env::var("AGENTKIT_STORE_TOKEN").unwrap();
    let ready = std::env::var("AGENTKIT_STORE_READY_PATH").unwrap();
    let store = SqliteStore::open(path, 2).unwrap();
    store.submit(task("ackrun", "scope")).await.unwrap();
    let lease = store.claim(claim(1000)).await.unwrap().unwrap().lease;
    store
        .append_events(lease.fence(), batch(&token, 0, 12), 1001)
        .await
        .unwrap();
    std::fs::write(ready, b"ready").unwrap();
    loop {
        std::thread::park();
    }
}

/// Drive one acknowledged-batch child to a killed writer, with or without the
/// fsync no-op armed, and assert the whole batch survived: acknowledged data
/// is wholly present after the writer dies, in order, and the reopened
/// handle (whose open runs schema validation and integrity checks) is clean.
///
/// Platform note: whether the commit's fsync actually fired cannot be
/// observed from user space on macOS — every read path (even `F_NOCACHE`) is
/// served from or flushes the unified buffer cache, so a cache-bypassing
/// disk inspection needs root and a power-cut rig this cell cannot own. What
/// this cell pins instead is the observable half of the contract: the
/// acknowledged batch is wholly serialized into the durable WAL image and
/// wholly readable by a writer-oblivious reader, never torn, both with and
/// without the fsync no-op armed. Whether the no-op itself is armed is
/// falsified by the `faults` unit tests, which read back the actual pragma.
async fn drive_acknowledged_killed_writer(fault: Option<&str>) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("agent.db");
    let ready = directory.path().join("ready");
    let token = format!(
        "ack-{}-{}",
        std::process::id(),
        fault.unwrap_or("full").replace('=', "-")
    );
    let mut child = spawn_fault_child(
        "durable_child_acknowledged_batch",
        directory.path(),
        &[
            ("AGENTKIT_STORE_READY_PATH", ready.to_str().unwrap()),
            ("AGENTKIT_STORE_TOKEN", &token),
            ("AGENTKIT_STORE_FAULT", fault.unwrap_or("")),
        ],
    );
    wait_for_file(&ready, &mut child, "acknowledged batch");
    child.kill().unwrap();
    child.wait().unwrap();
    // The acknowledged batch left the caller through the durable WAL image:
    // every event body the caller saw as committed is present in it.
    #[cfg(unix)]
    {
        let on_storage =
            read_bypassing_cache(&directory.path().join("agent.db-wal")).expect("wal readable");
        for index in 0..12 {
            let needle = format!("{token}-{index}");
            assert!(
                on_storage
                    .windows(needle.len())
                    .any(|window| window == needle.as_bytes()),
                "acknowledged event {needle} never reached the durable WAL image ({fault:?})"
            );
        }
    }
    // And a reader that never knew the writer agrees: all twelve, whole,
    // never torn, outbox in lockstep with the event log.
    let store = SqliteStore::open(&path, 2).unwrap();
    let events = store.events("ackrun", None, 20).await.unwrap();
    assert_eq!(
        events.len(),
        12,
        "torn or lost acknowledged batch ({fault:?})"
    );
    for (index, stored) in events.iter().enumerate() {
        assert_eq!(
            stored.event,
            event(index as u64, &format!("{token}-{index}"))
        );
    }
    let payloads: Vec<String> = store
        .outbox()
        .await
        .unwrap()
        .iter()
        .map(|record| record.payload.to_string())
        .collect();
    assert!(
        payloads
            .iter()
            .filter(|payload| payload.contains(&token))
            .count()
            == 12,
        "outbox disagrees with the event log: {payloads:?}"
    );
}

#[tokio::test]
async fn acknowledged_batch_survives_killed_writer() {
    drive_acknowledged_killed_writer(None).await;
}

#[tokio::test]
async fn acknowledged_batch_survives_killed_writer_with_fsync_noop() {
    // Simulated durability loss: the commit's fsync is a no-op, the writer
    // dies anyway. The acknowledged batch is still wholly present or this
    // fails — the acknowledged/unacknowledged boundary is honored even when
    // the sync itself is lost.
    drive_acknowledged_killed_writer(Some("fsync-off")).await;
}

/// Child body: acknowledges one anchor event, then abandons a large batch to
/// the actor and aborts the process mid-flight; whether the batch committed
/// before the abort is a race the parent never gets to observe directly.
#[tokio::test]
async fn fault_child_killed_before_acknowledgment() {
    let Ok(path) = std::env::var("AGENTKIT_STORE_DB_PATH") else {
        return;
    };
    let token = std::env::var("AGENTKIT_STORE_TOKEN").unwrap();
    let ready = std::env::var("AGENTKIT_STORE_READY_PATH").unwrap();
    let store = SqliteStore::open(path, 2).unwrap();
    store.submit(task("racerun", "scope")).await.unwrap();
    let lease = store.claim(claim(1000)).await.unwrap().unwrap().lease;
    store
        .append_events(
            lease.fence(),
            vec![event(0, &format!("{token}-anchor"))],
            1001,
        )
        .await
        .unwrap();
    let pending = tokio::spawn({
        let store = store.clone();
        let token = token.clone();
        async move {
            store
                .append_events(lease.fence(), batch(&token, 1, 24), 1001)
                .await
        }
    });
    // Yield once so the actor has (probably) started the batch, then die
    // without ever awaiting it: the batch is unacknowledged by construction.
    tokio::time::sleep(Duration::from_micros(200)).await;
    pending.abort();
    std::fs::write(ready, b"ready").unwrap();
    std::process::abort();
}

#[tokio::test]
async fn unacknowledged_batch_is_wholly_present_or_wholly_absent_never_torn() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("agent.db");
    let ready = directory.path().join("ready");
    let token = format!("race-{}", std::process::id());
    let mut child = spawn_fault_child(
        "fault_child_killed_before_acknowledgment",
        directory.path(),
        &[
            ("AGENTKIT_STORE_READY_PATH", ready.to_str().unwrap()),
            ("AGENTKIT_STORE_TOKEN", &token),
        ],
    );
    wait_for_file(&ready, &mut child, "abandoned batch");
    child.wait().unwrap();
    // Whatever side of the commit the abort caught the batch on, reopen sees
    // the boundary honestly: the acknowledged anchor always, and the batch
    // either complete (24, in order) or absent (0) — never torn, and the
    // outbox agrees with the event log either way.
    let store = SqliteStore::open(&path, 2).unwrap();
    let events = store.events("racerun", None, 40).await.unwrap();
    assert_eq!(events[0].event, event(0, &format!("{token}-anchor")));
    let batch: Vec<&_> = events
        .iter()
        .skip(1)
        .filter(|stored| {
            stored.event["eventId"]
                .as_str()
                .is_some_and(|id| id.starts_with(&token))
        })
        .collect();
    assert!(
        batch.is_empty() || batch.len() == 24,
        "torn batch: {} of 24",
        batch.len()
    );
    for (index, stored) in batch.iter().enumerate() {
        assert_eq!(
            stored.event,
            event(1 + index as u64, &format!("{token}-{index}"))
        );
    }
    let batch_outbox = store
        .outbox()
        .await
        .unwrap()
        .iter()
        .filter(|record| {
            record.payload.to_string().contains(&token)
                && !record.payload.to_string().contains("anchor")
        })
        .count();
    assert_eq!(batch_outbox, batch.len(), "outbox and event log diverged");
}
