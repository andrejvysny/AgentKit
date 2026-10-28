//! B1 regression: execution ownership survives caller cancellation/drop.
//!
//! The core tool harness completes its budget `select!` by DROPPING the tool
//! future; `run_command` must then keep the workspace lease blocked inside
//! its detached driver until the process group is dead, the survivor recheck
//! is done and (if survivors remained) quarantine is published — the release
//! must be the driver's act, never `WorkspaceLease::drop` firing from a
//! dropped tool future (ADR 0020).
//!
//! Every test drives the ACTUAL contributed `run_command` (same wiring as
//! `contributor.rs`) with a controlled child, and reaps every marked process
//! through a Drop guard keyed by the child's own `AGENTKIT_EXEC_ID` marker.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::id as process_id;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agentkit_core::{Tool, ToolContext, ToolLimits};
use agentkit_host::{ContributedTool, ToolContributionContext, ToolContributor};
use agentkit_toolpacks_coding::coordination::{
    AcquireWorkspaceInput, LeaseHolder, default_in_process_coordinator,
};
use agentkit_toolpacks_coding::process::live_executions;
use agentkit_toolpacks_coding::{
    CodingLimitsOverride, CodingToolPack, CodingToolPackOptions, CodingWorkspace, SandboxMode,
    create_coding_tool_pack,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

const POLL_MS: u64 = 20;

/// Pids whose `/proc/<pid>/environ` carries exactly this `AGENTKIT_EXEC_ID`.
/// Every descendant inherits the marker, including ones that left the
/// process group (setsid); a zombie's environ reads empty, so killed-but-
/// unreaped processes do not count as alive here.
fn marked_pids(exec_id: &str) -> Vec<i32> {
    let want = format!("AGENTKIT_EXEC_ID={exec_id}");
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return found;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let Ok(raw) = std::fs::read(entry.path().join("environ")) else {
            continue;
        };
        if raw
            .split(|byte| *byte == 0)
            .any(|field| field == want.as_bytes())
            && let Ok(pid) = name.parse::<i32>()
        {
            found.push(pid);
        }
    }
    found
}

/// Process-group id from `/proc/<pid>/stat` (None once the pid is gone).
fn pgrp(pid: i32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, rest) = stat.rsplit_once(") ")?;
    rest.split_whitespace().nth(2)?.parse().ok()
}

async fn poll_until(what: &str, budget: Duration, cond: impl Fn() -> bool) {
    let deadline = Instant::now() + budget;
    while !cond() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what} (marked: {:?})",
            marked_pids_untagged()
        );
        tokio::time::sleep(Duration::from_millis(POLL_MS)).await;
    }
}

// Only used for the failure message of `poll_until`; the condition itself is
// always passed explicitly.
thread_local! {
    static LAST_EXEC_ID: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
}

fn marked_pids_untagged() -> Vec<i32> {
    LAST_EXEC_ID.with(|id| marked_pids(&id.borrow()))
}

fn remember_exec_id(exec_id: &str) {
    LAST_EXEC_ID.with(|id| *id.borrow_mut() = exec_id.to_owned());
}

/// Kills + waits for every process marked with the harness's exec id when the
/// test ends, so no test — pass or fail — leaks a process.
struct ReapGuard {
    id_file: PathBuf,
    root: PathBuf,
}

impl Drop for ReapGuard {
    fn drop(&mut self) {
        let exec_id = std::fs::read_to_string(&self.id_file).unwrap_or_default();
        for _ in 0..5 {
            let pids = marked_pids(&exec_id);
            if pids.is_empty() {
                break;
            }
            for pid in &pids {
                unsafe { libc::kill(*pid, libc::SIGKILL) };
            }
            let deadline = Instant::now() + Duration::from_secs(5);
            while !marked_pids(&exec_id).is_empty() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(POLL_MS));
            }
        }
        let leaked = marked_pids(&exec_id);
        assert!(
            leaked.is_empty(),
            "execution-ownership test leaked processes: {leaked:?}"
        );
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

struct Harness {
    root: PathBuf,
    id_file: PathBuf,
    pack: Arc<CodingToolPack>,
    // Constructed last so nothing else can unroll the stack while it runs.
    _reaper: ReapGuard,
}

impl Harness {
    async fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "agentkit-exec-ownership-{}-{}-{}",
            process_id(),
            uuid::Uuid::new_v4().simple(),
            tag
        ));
        std::fs::create_dir_all(&root).unwrap();
        let root = std::fs::canonicalize(&root).unwrap();
        let pack = create_coding_tool_pack(CodingToolPackOptions {
            workspace: CodingWorkspace {
                root: root.to_string_lossy().into_owned(),
            },
            limits: CodingLimitsOverride {
                // TERM-ignoring children must escalate to SIGKILL quickly; the
                // assertions wait on the group dying, so the grace dominates.
                command_kill_grace_ms: Some(150),
                ..CodingLimitsOverride::default()
            },
            allow_sensitive: false,
            clock: None,
            sandbox: SandboxMode::Off,
            network: false,
            allow_git_writes: false,
            test_commands: vec![],
            platform: Some("linux".to_owned()),
            sandbox_exec_path: None,
            // Per-pack: the gates are per-root, but isolation also keeps the
            // probes honest about who holds the lease.
            coordinator: Some(default_in_process_coordinator()),
            coordination_wait_ms: Some(500),
            permission: None,
        })
        .await
        .unwrap();
        let id_file = root.join("exec-id");
        Self {
            root: root.clone(),
            id_file,
            pack,
            _reaper: ReapGuard {
                id_file: root.join("exec-id"),
                root: root.clone(),
            },
        }
    }

    /// A command that publishes its inherited exec marker before running
    /// `body`, so the test can name the exact execution it is watching.
    fn command(&self, body: &str) -> String {
        format!(
            "printf %s \"$AGENTKIT_EXEC_ID\" > '{}'; {body}",
            self.id_file.display()
        )
    }

    /// The contributed `run_command`, exactly as a host registers it.
    async fn run_command(&self) -> Arc<dyn Tool> {
        let context = ToolContributionContext {
            durable: None,
            chat_id: Some("chat-1".into()),
            run_id: "run-1".into(),
            scope_id: None,
            actor: Value::Null,
            actor_id: None,
            bindings: vec![],
            limits: ToolLimits {
                profile: "small".into(),
                max_bytes: 16384,
                max_items: None,
            },
            cancellation: CancellationToken::new(),
            has_primary_binding: true,
            timeout_ms: 30_000,
        };
        for tool in self.pack.contribute(&context).await.unwrap() {
            if let ContributedTool::DirectWrite(tool) = tool
                && tool.definition().name == "run_command"
            {
                return tool;
            }
        }
        panic!("run_command was not contributed");
    }

    /// Wait for the child to have published its exec id, and remember it for
    /// diagnostics.
    async fn exec_id(&self) -> String {
        let path = self.id_file.clone();
        poll_until(
            "the child to publish its exec id",
            Duration::from_secs(20),
            || std::fs::read_to_string(&path).is_ok_and(|id| !id.is_empty()),
        )
        .await;
        let exec_id = std::fs::read_to_string(&self.id_file).unwrap();
        remember_exec_id(&exec_id);
        exec_id
    }
}

fn tool_context(cancellation: CancellationToken) -> ToolContext {
    ToolContext {
        suspension: None,
        run_id: "run-ownership".to_owned(),
        tool_call_id: Some("call-1".to_owned()),
        attempt_id: None,
        chat_id: Some("chat-1".to_owned()),
        scope_id: None,
        user_id: None,
        bindings: vec![],
        limits: ToolLimits::default(),
        cancellation,
        dispatch_guard: None,
        metadata: serde_json::Map::new(),
    }
}

/// One lease probe: Ok = the gate admitted us (release the lease by dropping
/// it); Err(code) = refused (e.g. `workspace_busy`).
async fn try_acquire(pack: &CodingToolPack, root: &Path, wait_ms: u64) -> Result<(), String> {
    match pack
        .env()
        .coordinator
        .acquire_workspace(AcquireWorkspaceInput {
            root: root.to_string_lossy().into_owned(),
            holder: LeaseHolder {
                chat_id: Some("prober".into()),
                run_id: "probe".into(),
                tool_call_id: None,
                tool: "probe".into(),
                label: "ownership probe".into(),
            },
            cancellation: None,
            max_wait_ms: Some(wait_ms),
            execution_id: None,
        })
        .await
    {
        // The in-process lease releases on drop; the probe never holds it.
        Ok(_lease) => Ok(()),
        Err(error) => Err(error.code),
    }
}

/// Same as the repro, structured so the drop is explicit and deterministic:
/// poll until the marker file exists, then take the 300 ms select branch and
/// drop the future mid-run.
async fn poll_past_spawn_then_drop(tool: Arc<dyn Tool>, input: Value, harness: &Harness) {
    let mut future = Box::pin(tool.execute(tool_context(CancellationToken::new()), input));
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let spawned = std::fs::read_to_string(&harness.id_file).is_ok_and(|id| !id.is_empty());
        if spawned {
            break;
        }
        assert!(Instant::now() < deadline, "the child never spawned");
        if tokio::time::timeout(Duration::from_millis(50), future.as_mut())
            .await
            .is_ok()
        {
            panic!("the tool completed before the drop window opened");
        }
    }
    // The core harness's timeout branch: this completes the select! and the
    // tool future is dropped while the command is still running.
    tokio::select! {
        biased;
        _ = tokio::time::sleep(Duration::from_millis(300)) => {}
        _ = future.as_mut() => panic!("the tool completed despite the 300 ms budget branch"),
    }
    drop(future);
}

/// After a dropped caller, the workspace may only be admitted once the whole
/// group is dead: never while a marked process is alive. Returns once the
/// driver has released (probe admitted) and the registry is empty.
async fn assert_gate_after_drop(harness: &Harness, exec_id: &str) {
    let mut admitted_while_alive = false;
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut admitted = false;
    while !admitted {
        assert!(Instant::now() < deadline, "the driver never released");
        let alive_before = marked_pids(exec_id);
        if try_acquire(&harness.pack, &harness.root, 100).await.is_ok() {
            if !marked_pids(exec_id).is_empty() {
                admitted_while_alive = true;
            }
            admitted = true;
        } else if !alive_before.is_empty() {
            // Still cleaning up: the gate MUST stay closed while the child
            // lives — this is the B1 assertion.
        }
    }
    assert!(
        !admitted_while_alive,
        "B1: the workspace was admitted while a marked child was still alive"
    );
    poll_until("the driver to unregister", Duration::from_secs(10), || {
        !live_executions()
            .iter()
            .any(|live| live.execution_id == exec_id)
    })
    .await;
}

#[tokio::test]
async fn dropped_tool_future_keeps_the_lease_until_the_group_is_dead() {
    let harness = Harness::new("budget-drop").await;
    let tool = harness.run_command().await;
    // `timeoutSec: 1` keeps the driver's own timeout close after the drop, so
    // the group dies through the driver, not through natural exit.
    let input = json!({ "command": harness.command("trap '' TERM; sleep 30"), "timeoutSec": 1 });
    poll_past_spawn_then_drop(Arc::clone(&tool), input, &harness).await;
    let exec_id = harness.exec_id().await;
    assert!(
        !marked_pids(&exec_id).is_empty(),
        "the child must be alive at the drop"
    );
    // The driver, not the dropped caller, owns the execution now.
    poll_until("the driver to register", Duration::from_secs(5), || {
        live_executions()
            .iter()
            .any(|live| live.execution_id == exec_id)
    })
    .await;
    assert_gate_after_drop(&harness, &exec_id).await;
    harness.pack.shutdown().await;
}

#[tokio::test]
async fn caller_dropped_without_cancellation_holds_the_lease_until_exit() {
    let harness = Harness::new("caller-drop").await;
    let tool = harness.run_command().await;
    // No cancellation: the command runs to its natural exit (~6 s); the lease
    // must stay held across the whole run even though nobody awaits the tool.
    let input = json!({ "command": harness.command("sleep 6"), "timeoutSec": 30 });
    let mut future = Box::pin(tool.execute(tool_context(CancellationToken::new()), input));
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if std::fs::read_to_string(&harness.id_file).is_ok_and(|id| !id.is_empty()) {
            break;
        }
        assert!(Instant::now() < deadline, "the child never spawned");
        if tokio::time::timeout(Duration::from_millis(50), future.as_mut())
            .await
            .is_ok()
        {
            panic!("the tool completed before the drop");
        }
    }
    let exec_id = harness.exec_id().await;
    drop(future);
    // Within the window: the child lives and a second writer is refused.
    assert!(
        !marked_pids(&exec_id).is_empty(),
        "the child must be alive in the window"
    );
    let probe = try_acquire(&harness.pack, &harness.root, 250).await;
    assert_eq!(
        probe.err().as_deref(),
        Some("workspace_busy"),
        "B1: a dropped caller must not open the gate; the lease is the driver's"
    );
    // The child later exits; the driver reaps, releases, and the gate opens.
    assert_gate_after_drop(&harness, &exec_id).await;
    harness.pack.shutdown().await;
}

#[tokio::test]
async fn explicit_cancellation_reports_aborted_and_kills_the_group() {
    let harness = Harness::new("cancel").await;
    let tool = harness.run_command().await;
    let token = CancellationToken::new();
    let input = json!({ "command": harness.command("sleep 30"), "timeoutSec": 30 });
    let cancellation = token.clone();
    let running = tokio::spawn(async move {
        tool.execute(tool_context(cancellation), input)
            .await
            .unwrap()
    });
    let exec_id = harness.exec_id().await;
    poll_until("the child to be alive", Duration::from_secs(10), || {
        !marked_pids(&exec_id).is_empty()
    })
    .await;
    token.cancel();
    let result = tokio::time::timeout(Duration::from_secs(30), running)
        .await
        .expect("the cancelled call never returned")
        .expect("the tool task panicked");
    assert!(!result.ok);
    assert_eq!(result.summary.as_deref(), Some("aborted"));
    assert!(
        result
            .model_data
            .as_ref()
            .and_then(Value::as_str)
            .is_some_and(|text| text.contains("[aborted; process group killed]")),
        "aborted footer missing: {:?}",
        result.model_data
    );
    let details = result.data.clone().unwrap();
    assert_eq!(details["timedOut"], json!(false));
    assert_eq!(details["teardown"]["survivors"], json!([]));
    // The group is gone by the time the aborted result is reported.
    assert!(
        marked_pids(&exec_id).is_empty(),
        "B1: survivors {:?} after the aborted result",
        marked_pids(&exec_id)
    );
    harness.pack.shutdown().await;
}

#[tokio::test]
async fn pack_shutdown_drains_a_running_command() {
    let harness = Harness::new("shutdown").await;
    let tool = harness.run_command().await;
    let input = json!({ "command": harness.command("sleep 117"), "timeoutSec": 60 });
    let running = tokio::spawn(async move {
        tool.execute(tool_context(CancellationToken::new()), input)
            .await
            .unwrap()
    });
    let exec_id = harness.exec_id().await;
    poll_until("the child to be alive", Duration::from_secs(10), || {
        !marked_pids(&exec_id).is_empty()
    })
    .await;
    poll_until("the driver to register", Duration::from_secs(5), || {
        live_executions()
            .iter()
            .any(|live| live.execution_id == exec_id)
    })
    .await;
    harness.pack.shutdown().await;
    // Shutdown returned: nothing is running and nothing is in flight (for
    // this workspace — other tests' workspaces are not ours to drain).
    assert!(
        marked_pids(&exec_id).is_empty(),
        "B1: shutdown returned with survivors {:?}",
        marked_pids(&exec_id)
    );
    assert!(
        !live_executions()
            .iter()
            .any(|live| live.execution_id == exec_id),
        "a driver outlived shutdown"
    );
    let result = tokio::time::timeout(Duration::from_secs(30), running)
        .await
        .expect("the drained call never returned")
        .expect("the tool task panicked");
    assert!(!result.ok);
    assert!(
        result
            .summary
            .as_deref()
            .is_some_and(|summary| summary.starts_with("killed by SIG")),
        "expected a killed-by-signal summary, got {:?}",
        result.summary
    );
    harness.pack.dispose().await;
}

#[tokio::test]
async fn detached_descendant_is_killed_with_no_survivors_and_no_quarantine() {
    let harness = Harness::new("detached").await;
    let tool = harness.run_command().await;
    // A grandchild that leaves the process group via setsid; the inherited
    // marker is what finds it. The 2 s command timeout makes the driver tear
    // everything down.
    let input = json!({
        "command": harness.command("setsid sleep 118 & sleep 119"),
        "timeoutSec": 2,
    });
    let running = tokio::spawn(async move {
        tool.execute(tool_context(CancellationToken::new()), input)
            .await
            .unwrap()
    });
    let exec_id = harness.exec_id().await;
    // Observe BOTH the in-group sleeper and the group-detached grandchild
    // while they live: their process-group ids differ (setsid's child leads
    // a group of its own), so the marked set spans two distinct pgids.
    poll_until("the detached descendant", Duration::from_secs(15), || {
        let pids = marked_pids(&exec_id);
        let groups: std::collections::HashSet<i32> =
            pids.iter().filter_map(|pid| pgrp(*pid)).collect();
        pids.len() >= 2 && groups.len() >= 2
    })
    .await;
    let result = tokio::time::timeout(Duration::from_secs(30), running)
        .await
        .expect("the timed-out call never returned")
        .expect("the tool task panicked");
    assert!(!result.ok);
    assert_eq!(result.data.clone().unwrap()["timedOut"], json!(true));
    assert_eq!(
        result.data.clone().unwrap()["teardown"]["survivors"],
        json!([])
    );
    assert_eq!(result.data.unwrap()["teardown"]["scan"], json!("ok"));
    assert!(
        result
            .model_data
            .as_ref()
            .and_then(Value::as_str)
            .is_some_and(|text| text.contains("[timed out after 2s; process group killed")),
        "timeout footer missing: {:?}",
        result.model_data
    );
    poll_until(
        "the group and the detached descendant to die",
        Duration::from_secs(20),
        || marked_pids(&exec_id).is_empty(),
    )
    .await;
    // No survivors meant no quarantine: the gate is open again right away.
    assert_gate_after_drop(&harness, &exec_id).await;
    harness.pack.shutdown().await;
}
