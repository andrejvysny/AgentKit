#![cfg(target_os = "linux")]
//! Command supervisor tests: run the supervisor directly (no vsock) and
//! verify spawn, exit codes, signals, group-kill on timeout, rlimits,
//! truncation, cancellation and uid-drop policy (§8.3, T10).

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use agentkit_sandbox_contracts::api::{ExecArgv, ExecEventKind};
use agentkit_sandbox_contracts::grant::GuestEnvironment;
use agentkit_sandbox_contracts::ids::ExecutionId;
use agentkit_sandbox_contracts::path::GuestPath;
use agentkit_sandbox_guest::fsops::{GuestFs, Roots};
use agentkit_sandbox_guest::supervisor::{SpawnRequest, Supervisor};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use tokio::sync::mpsc;

struct Harness {
    _tmp: tempfile::TempDir,
    sup: Arc<Supervisor>,
    rx: mpsc::UnboundedReceiver<(ExecutionId, ExecEventKind)>,
}

fn harness(allow_root: bool) -> Harness {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let ws = tmp.path().join("workspace");
    let home = tmp.path().join("home");
    let gstmp = tmp.path().join("tmp");
    for d in [&ws, &home, &gstmp] {
        std::fs::create_dir_all(d).expect("root");
    }
    let fs = Arc::new(GuestFs::open(Roots::for_tests(ws, home, gstmp)).expect("open roots"));
    let (tx, rx) = mpsc::unbounded_channel();
    Harness {
        _tmp: tmp,
        sup: Arc::new(Supervisor::new(allow_root, fs, tx)),
        rx,
    }
}

fn req(
    execution: &str,
    argv: ExecArgv,
    timeout_sec: u32,
    max_output: u64,
    max_procs: u32,
) -> SpawnRequest {
    SpawnRequest {
        execution_id: ExecutionId::new(execution).expect("id"),
        argv,
        cwd: GuestPath::parse("/workspace").expect("cwd"),
        environment: GuestEnvironment::new(),
        timeout_sec,
        max_output_bytes: max_output,
        max_processes: max_procs,
        run_as_uid: unsafe { libc::geteuid() },
        run_as_gid: unsafe { libc::getgid() },
    }
}

/// Drive `spawn` + collect events until Exited (or failure) with a deadline.
async fn run(h: &mut Harness, r: SpawnRequest) -> (Vec<ExecEventKind>, Option<u32>) {
    let id = r.execution_id.clone();
    h.sup.spawn(r).expect("admitted");
    let mut events = Vec::new();
    let mut pid = None;
    let collect = async {
        while let Some((eid, kind)) = h.rx.recv().await {
            if eid != id {
                continue;
            }
            let done = matches!(kind, ExecEventKind::Exited { .. });
            if let ExecEventKind::Spawned { guest_pid, .. } = &kind {
                pid = Some(*guest_pid);
            }
            events.push(kind);
            if done {
                // allow a trailing Uncertain (descendants) to arrive
                tokio::time::sleep(Duration::from_millis(150)).await;
                while let Ok((_, kind)) = h.rx.try_recv() {
                    if kind_is_for(&kind, &eid) {
                        events.push(kind);
                    }
                }
                return;
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(20), collect)
        .await
        .expect("execution finished in time");
    (events, pid)
}

fn kind_is_for(_k: &ExecEventKind, _id: &ExecutionId) -> bool {
    true
}

fn stdout_of(events: &[ExecEventKind]) -> Vec<u8> {
    let mut out = Vec::new();
    for e in events {
        if let ExecEventKind::StdoutChunk { bytes_b64 } = e {
            out.extend(STANDARD.decode(bytes_b64).expect("b64"));
        }
    }
    out
}

fn exited(events: &[ExecEventKind]) -> (Option<i32>, Option<i32>, bool) {
    events
        .iter()
        .find_map(|e| match e {
            ExecEventKind::Exited {
                exit_code,
                signal,
                timed_out,
                ..
            } => Some((*exit_code, *signal, *timed_out)),
            _ => None,
        })
        .expect("Exited event present")
}

fn has(events: &[ExecEventKind], pred: impl Fn(&ExecEventKind) -> bool) -> bool {
    events.iter().any(pred)
}

#[tokio::test]
async fn echo_via_direct_printf() {
    if !Path::new("/usr/bin/printf").exists() {
        return; // environment without coreutils printf
    }
    let mut h = harness(false);
    let (events, pid) = run(
        &mut h,
        req(
            "e1",
            ExecArgv::Direct {
                executable: "/usr/bin/printf".into(),
                args: vec!["hello-%s\n".into(), "world".into()],
            },
            10,
            1 << 20,
            32,
        ),
    )
    .await;
    assert!(has(&events, |e| matches!(
        e,
        ExecEventKind::Accepted { .. }
    )));
    assert!(has(&events, |e| matches!(e, ExecEventKind::Spawned { .. })));
    assert_eq!(stdout_of(&events), b"hello-world\n");
    assert_eq!(exited(&events), (Some(0), None, false));
    assert!(pid.unwrap() > 0);
}

#[tokio::test]
async fn nonzero_exit_code_reported() {
    if !Path::new("/usr/bin/false").exists() {
        return;
    }
    let mut h = harness(false);
    let (events, _) = run(
        &mut h,
        req(
            "e2",
            ExecArgv::Direct {
                executable: "/usr/bin/false".into(),
                args: vec![],
            },
            10,
            1 << 20,
            32,
        ),
    )
    .await;
    assert_eq!(exited(&events).0, Some(1));
}

#[tokio::test]
async fn signal_death_reports_signal() {
    if !Path::new("/bin/bash").exists() {
        return;
    }
    let mut h = harness(false);
    let (events, _) = run(
        &mut h,
        req(
            "e3",
            ExecArgv::Shell {
                command: "kill -TERM $$".into(),
            },
            10,
            1 << 20,
            32,
        ),
    )
    .await;
    let (code, signal, timed_out) = exited(&events);
    assert_eq!(signal, Some(15), "code={code:?}");
    assert!(!timed_out);
}

#[tokio::test]
async fn timeout_group_kills_background_sleep() {
    if !Path::new("/bin/bash").exists() || !Path::new("/usr/bin/sleep").exists() {
        return;
    }
    let mut h = harness(false);
    let (events, pid) = run(
        &mut h,
        req(
            "e4",
            ExecArgv::Shell {
                command: "sleep 40 & sleep 40".into(),
            },
            1,
            1 << 20,
            32,
        ),
    )
    .await;
    let (_, _, timed_out) = exited(&events);
    assert!(timed_out, "must report timed_out");
    // Give stragglers a moment, then prove no sleep in that group survives.
    tokio::time::sleep(Duration::from_millis(400)).await;
    let pgid = pid.expect("spawned pid");
    let survivors = sleep_members(pgid);
    assert!(survivors.is_empty(), "surviving sleeps: {survivors:?}");
}

/// /proc scan: pids in `pgid` whose comm is `sleep`.
fn sleep_members(pgid: u32) -> Vec<u32> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir("/proc") else {
        return out;
    };
    for ent in rd.flatten() {
        let Ok(num) = ent.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{num}/stat")) else {
            continue;
        };
        let Some((head, fields)) = stat.rsplit_once(')') else {
            continue;
        };
        if !head.ends_with("(sleep") {
            continue;
        }
        if let Some(g) = fields.split_whitespace().nth(2)
            && let Ok(g) = g.parse::<u32>()
            && g == pgid
        {
            out.push(num);
        }
    }
    out
}

#[tokio::test]
async fn rlimit_nproc_bounds_fork_bomb() {
    if !Path::new("/bin/bash").exists() {
        return;
    }
    let mut h = harness(false);
    let (events, pid) = run(
        &mut h,
        req(
            "e5",
            ExecArgv::Shell {
                command: "while :; do : & done".into(),
            },
            2,
            1 << 20,
            8,
        ),
    )
    .await;
    let (_, _, timed_out) = exited(&events);
    assert!(timed_out, "fork bomb must hit the timeout");
    tokio::time::sleep(Duration::from_millis(400)).await;
    let survivors = group_members(pid.expect("pid"));
    // RLIMIT_NPROC per-uid plus group kill: bound far below a runaway bomb.
    assert!(survivors.len() < 64, "unbounded survivors: {survivors:?}");
}

fn group_members(pgid: u32) -> Vec<u32> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir("/proc") else {
        return out;
    };
    for ent in rd.flatten() {
        let Ok(num) = ent.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{num}/stat")) else {
            continue;
        };
        if let Some(fields) = stat.rsplit(')').next()
            && let Some(g) = fields.split_whitespace().nth(2)
            && let Ok(g) = g.parse::<u32>()
            && g == pgid
        {
            out.push(num);
        }
    }
    out
}

#[tokio::test]
async fn output_truncation_flag_with_tiny_cap() {
    if !Path::new("/usr/bin/printf").exists() {
        return;
    }
    let mut h = harness(false);
    let blob = "Z".repeat(5000);
    let (events, _) = run(
        &mut h,
        req(
            "e6",
            ExecArgv::Direct {
                executable: "/usr/bin/printf".into(),
                args: vec![blob],
            },
            10,
            1024,
            32,
        ),
    )
    .await;
    assert!(
        has(&events, |e| matches!(
            e,
            ExecEventKind::Truncated {
                stream: agentkit_sandbox_contracts::api::StreamKind::Stdout,
                ..
            }
        )),
        "expected Truncated event, got {events:?}"
    );
    assert_eq!(stdout_of(&events).len(), 1024);
}

#[tokio::test]
async fn cancel_running_emits_signal_exit() {
    if !Path::new("/bin/bash").exists() {
        return;
    }
    let mut h = harness(false);
    let r = req(
        "e7",
        ExecArgv::Shell {
            command: "sleep 40".into(),
        },
        60,
        1 << 20,
        32,
    );
    let id = r.execution_id.clone();
    h.sup.spawn(r).expect("admitted");
    // wait for Spawned
    let mut spawned = false;
    while let Some((_, kind)) = h.rx.recv().await {
        if matches!(kind, ExecEventKind::Spawned { .. }) {
            spawned = true;
            break;
        }
    }
    assert!(spawned);
    h.sup.cancel(&id);
    let mut signal = None;
    let t = tokio::time::timeout(Duration::from_secs(15), async {
        while let Some((_, kind)) = h.rx.recv().await {
            if let ExecEventKind::Exited {
                signal: s,
                timed_out,
                ..
            } = kind
            {
                assert!(!timed_out);
                signal = s;
                return;
            }
        }
    })
    .await;
    assert!(t.is_ok(), "cancel did not terminate execution");
    assert!(signal == Some(15) || signal == Some(9), "signal {signal:?}");
}

#[tokio::test]
async fn cancel_finished_is_noop() {
    if !Path::new("/usr/bin/true").exists() {
        return;
    }
    let mut h = harness(false);
    let (events, _) = run(
        &mut h,
        req(
            "e8",
            ExecArgv::Direct {
                executable: "/usr/bin/true".into(),
                args: vec![],
            },
            10,
            1 << 20,
            32,
        ),
    )
    .await;
    assert_eq!(exited(&events), (Some(0), None, false));
    let id = ExecutionId::new("e8").unwrap();
    h.sup.cancel(&id); // must not panic, must not emit more events
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(h.rx.try_recv().is_err());
}

#[tokio::test]
async fn uid_drop_semantics() {
    if !Path::new("/usr/bin/id").exists() {
        return;
    }
    let me = unsafe { libc::geteuid() };
    if me == 0 {
        // Privileged: drop to 1000 and verify `id -u`.
        let mut h = harness(false);
        let mut r = req(
            "u1",
            ExecArgv::Direct {
                executable: "/usr/bin/id".into(),
                args: vec!["-u".into()],
            },
            10,
            1 << 20,
            32,
        );
        r.run_as_uid = 1000;
        r.run_as_gid = 1000;
        let (events, _) = run(&mut h, r).await;
        assert_eq!(String::from_utf8_lossy(&stdout_of(&events)).trim(), "1000");
    } else {
        // Unprivileged: requesting our own uid works and `id -u` matches;
        // requesting a foreign uid is refused before Accepted (EPERM class).
        let mut h = harness(false);
        let (events, _) = run(
            &mut h,
            req(
                "u2",
                ExecArgv::Direct {
                    executable: "/usr/bin/id".into(),
                    args: vec!["-u".into()],
                },
                10,
                1 << 20,
                32,
            ),
        )
        .await;
        assert_eq!(
            String::from_utf8_lossy(&stdout_of(&events)).trim(),
            me.to_string()
        );

        let mut r = req(
            "u3",
            ExecArgv::Direct {
                executable: "/usr/bin/id".into(),
                args: vec!["-u".into()],
            },
            10,
            1 << 20,
            32,
        );
        r.run_as_uid = 1000;
        r.run_as_gid = 1000;
        let err = h.sup.spawn(r).expect_err("foreign uid must be refused");
        assert_eq!(err.code, "not_started");
    }
}

#[tokio::test]
async fn root_refused_without_cmdline_flag() {
    let h = harness(false);
    let mut r = req(
        "g1",
        ExecArgv::Direct {
            executable: "/usr/bin/id".into(),
            args: vec![],
        },
        10,
        1 << 20,
        32,
    );
    r.run_as_uid = 0;
    r.run_as_gid = 0;
    let err = h.sup.spawn(r).expect_err("uid 0 must be refused");
    assert_eq!(err.code, "root_denied");
}

#[tokio::test]
async fn direct_executable_outside_path_dirs_refused() {
    let h = harness(false);
    let err = h
        .sup
        .spawn(req(
            "b1",
            ExecArgv::Direct {
                executable: "/etc/evil".into(),
                args: vec![],
            },
            5,
            1 << 20,
            32,
        ))
        .expect_err("must refuse non-PATH executable");
    assert_eq!(err.code, "bad_executable");
}

#[tokio::test]
async fn events_are_per_execution_sequenced_by_sink_owner() {
    // The supervisor emits in causal order; sanity-check the ordering shape.
    if !Path::new("/usr/bin/true").exists() {
        return;
    }
    let mut h = harness(false);
    let (events, _) = run(
        &mut h,
        req(
            "o1",
            ExecArgv::Direct {
                executable: "/usr/bin/true".into(),
                args: vec![],
            },
            10,
            1 << 20,
            32,
        ),
    )
    .await;
    let kinds: Vec<&str> = events
        .iter()
        .map(|e| match e {
            ExecEventKind::Accepted { .. } => "accepted",
            ExecEventKind::Spawned { .. } => "spawned",
            ExecEventKind::Exited { .. } => "exited",
            _ => "other",
        })
        .collect();
    let expected = HashMap::from([("accepted", 0usize), ("spawned", 1), ("exited", 2)]);
    for (k, idx) in expected {
        let at = kinds
            .iter()
            .position(|x| *x == k)
            .unwrap_or_else(|| panic!("no {k}"));
        assert_eq!(at, idx, "order {kinds:?}");
    }
}
