//! `ensure`, `stop` and `inspect`: idempotence, generation fences and
//! stop proofs.

use std::os::unix::fs::{MetadataExt, symlink};

use agentkit_sandbox_proto::machine_dir_name;

use super::*;
use crate::naming::{LABEL_GENERATION, LABEL_MANAGED, LABEL_WORKSPACE};

impl Harness {
    /// The machine directory of `ws` at `generation`.
    fn machine_dir(&self, ws: &str, generation: u64) -> PathBuf {
        self.root
            .join("sockets")
            .join(machine_dir_name(ws, generation))
    }
}

#[tokio::test]
async fn ensure_creates_the_volumes_and_directory_then_starts() {
    let h = Harness::new();
    let id = ensured(h.ensure("ws-1", 1).await);
    let (state, labels, body) = h.fake.with(|s| {
        let c = s.containers.iter().find(|c| c.id == id).unwrap();
        (c.state.clone(), c.labels.clone(), c.body.clone())
    });
    assert_eq!(state, "running");
    assert_eq!(labels[LABEL_GENERATION], "1");
    assert_eq!(labels[LABEL_WORKSPACE], "ws-1");
    let dir = h.machine_dir("ws-1", 1);
    assert_eq!(std::fs::metadata(&dir).unwrap().mode() & 0o777, 0o711);
    assert_eq!(
        body["HostConfig"]["Mounts"][2]["Source"],
        dir.to_str().unwrap()
    );
    let volumes = h.fake.with(|s| s.volumes.clone());
    assert_eq!(
        volumes.keys().collect::<Vec<_>>(),
        ["aks-home-ws-1", "aks-ws-ws-1"]
    );
    assert_eq!(volumes["aks-ws-ws-1"].labels[LABEL_MANAGED], "1");
    let calls = h.fake.calls();
    let create = calls
        .iter()
        .position(|c| c.starts_with("create aks-"))
        .unwrap();
    assert!(calls[create + 1].starts_with("start "), "{calls:?}");
}

#[tokio::test]
async fn ensure_is_idempotent_and_serialized() {
    let h = Harness::new();
    let (first, second) = tokio::join!(h.ensure("ws-1", 1), h.ensure("ws-1", 1));
    let first = ensured_with_token(first);
    assert_eq!(ensured_with_token(second), first);
    assert_eq!(ensured_with_token(h.ensure("ws-1", 1).await), first);
    assert_eq!(h.fake.count("create aks-"), 1);
    assert_eq!(h.fake.count("start "), 1);
    assert_eq!(h.launcher().locks.len(), 0, "lock entries are dropped");
}

/// The token is placed in the container's environment, read back from it
/// on a repeated ensure (also after a launcher restart: nothing is stored),
/// and differs per container. It never reaches the log line.
#[tokio::test]
async fn each_container_gets_its_own_token_and_keeps_it() {
    let mut h = Harness::new();
    let (id, token) = ensured_with_token(h.ensure("ws-1", 1).await);
    let env = h.fake.with(|s| {
        let c = s.containers.iter().find(|c| c.id == id).unwrap();
        c.body["Env"].clone()
    });
    let entry = format!("AGENTKIT_CHANNEL_TOKEN={token}");
    assert_eq!(
        env.as_array()
            .unwrap()
            .iter()
            .filter(|e| e.as_str() == Some(entry.as_str()))
            .count(),
        1
    );
    h.restart();
    assert_eq!(
        ensured_with_token(h.ensure("ws-1", 1).await),
        (id, token.clone())
    );
    assert!(h.fake.count("env ") >= 1, "read back from the container");
    let state = std::fs::read_to_string(h.root.join("state/generations.json")).unwrap();
    assert!(!state.contains(&token), "the launcher stores no token");
    assert!(proven(&h.stop("ws-1", 1).await));
    let (_, next) = ensured_with_token(h.ensure("ws-1", 2).await);
    assert_ne!(next, token);
    let reply = h.ensure("ws-1", 2).await;
    let line = outcome_text(&reply);
    assert!(line.starts_with("ensured "), "{line}");
    assert!(!line.contains(&next), "the token is never logged: {line}");
}

/// A running container of the generation whose environment has no valid
/// token cannot be answered: `busy`, so the service stops it and moves on.
#[tokio::test]
async fn a_running_container_without_a_token_is_busy() {
    let h = Harness::new();
    h.fake.insert(
        "aks-ws-1-g1",
        "running",
        &[
            (LABEL_WORKSPACE, "ws-1"),
            (LABEL_GENERATION, "1"),
            (crate::naming::LABEL_IMAGE, DIGEST),
            (LABEL_MANAGED, "1"),
        ],
    );
    assert_eq!(code(&h.ensure("ws-1", 1).await), LauncherErrorCode::Busy);
    assert!(proven(&h.stop("ws-1", 1).await));
    ensured(h.ensure("ws-1", 2).await);
}

#[tokio::test]
async fn bad_requests_touch_nothing() {
    let h = Harness::new();
    let mismatch = h
        .simple(Request::Ensure {
            workspace_id: "ws-1".into(),
            generation: 1,
            image_digest: format!("{}0", &DIGEST[..DIGEST.len() - 1]),
            tier: tier(),
        })
        .await;
    assert_eq!(code(&mismatch), LauncherErrorCode::ImageMismatch);
    let odd_tier = h
        .simple(Request::Ensure {
            workspace_id: "ws-1".into(),
            generation: 1,
            image_digest: DIGEST.into(),
            tier: Tier { pids: 0, ..tier() },
        })
        .await;
    assert_eq!(code(&odd_tier), LauncherErrorCode::InvalidRequest);
    for ws in ["../etc", "", "WS", "a/b", &"a".repeat(65)] {
        assert_eq!(
            code(&h.ensure(ws, 1).await),
            LauncherErrorCode::InvalidRequest
        );
        assert_eq!(
            code(
                &h.simple(Request::Destroy {
                    workspace_id: ws.into()
                })
                .await
            ),
            LauncherErrorCode::InvalidRequest
        );
    }
    assert_eq!(
        code(&h.ensure("ws-1", 0).await),
        LauncherErrorCode::InvalidRequest
    );
    assert_eq!(
        code(&h.stop("ws-1", 0).await),
        LauncherErrorCode::InvalidRequest
    );
    assert!(h.fake.calls().is_empty(), "{:?}", h.fake.calls());
    assert_eq!(
        std::fs::read_dir(h.root.join("sockets")).unwrap().count(),
        0
    );
}

/// INV-5 and INV-8, across a launcher restart.
#[tokio::test]
async fn proven_generations_stay_refused_after_a_restart() {
    let mut h = Harness::new();
    ensured(h.ensure("ws-1", 1).await);
    assert!(proven(&h.stop("ws-1", 1).await));
    assert!(!h.machine_dir("ws-1", 1).exists());
    assert_eq!(code(&h.ensure("ws-1", 1).await), stale(1));
    h.restart();
    assert_eq!(code(&h.ensure("ws-1", 1).await), stale(1));
    ensured(h.ensure("ws-1", 2).await);
    assert_eq!(code(&h.ensure("ws-1", 1).await), stale(2));
    let inspected = h
        .simple(Request::Inspect {
            workspace_id: "ws-1".into(),
        })
        .await;
    assert_eq!(
        inspected,
        Reply::Inspected {
            running: Some(2),
            stopped_through: Some(1),
        }
    );
}

/// A generation below one a container was created for is refused, and
/// that one gets no second container, even when the first vanished
/// outside the launcher (INV-8; a generation runs once).
#[tokio::test]
async fn a_created_generation_is_never_created_again() {
    let mut h = Harness::new();
    let id = ensured(h.ensure("ws-1", 3).await);
    h.fake.with(|s| s.containers.retain(|c| c.id != id));
    h.restart();
    assert_eq!(code(&h.ensure("ws-1", 2).await), stale(3));
    assert_eq!(code(&h.ensure("ws-1", 3).await), LauncherErrorCode::Busy);
    assert_eq!(h.fake.count("create aks-"), 1);
    assert!(proven(&h.stop("ws-1", 3).await));
    ensured(h.ensure("ws-1", 4).await);
}

#[tokio::test]
async fn another_generation_is_never_replaced() {
    let h = Harness::new();
    let first = ensured(h.ensure("ws-1", 1).await);
    assert_eq!(code(&h.ensure("ws-1", 2).await), LauncherErrorCode::Busy);
    h.fake.set_state(&first, "exited");
    assert_eq!(code(&h.ensure("ws-1", 2).await), LauncherErrorCode::Busy);
    h.fake.insert(
        "aks-ws-1-g5",
        "running",
        &[(LABEL_WORKSPACE, "ws-1"), (LABEL_GENERATION, "5")],
    );
    assert_eq!(code(&h.ensure("ws-1", 4).await), stale(5));
    assert_eq!(h.fake.count("create aks-"), 1);
    assert_eq!(h.fake.count("remove "), 0);
}

#[tokio::test]
async fn a_dead_or_unstarted_generation_does_not_start_again() {
    let h = Harness::new();
    let id = ensured(h.ensure("ws-1", 1).await);
    h.fake.set_state(&id, "exited");
    assert_eq!(code(&h.ensure("ws-1", 1).await), LauncherErrorCode::Busy);
    h.fake.set_state(&id, "created");
    assert_eq!(code(&h.ensure("ws-1", 1).await), LauncherErrorCode::Busy);
    assert_eq!(h.fake.count("start "), 1);
    assert!(proven(&h.stop("ws-1", 1).await));
    ensured(h.ensure("ws-1", 2).await);
}

/// A container of the workspace whose generation cannot be read blocks
/// `ensure` and is covered by every stop: it is removed, and the stop is
/// proven only once it is gone.
#[tokio::test]
async fn an_unlabelled_generation_blocks_ensure_until_a_stop_removes_it() {
    let h = Harness::new();
    let stray = h.fake.insert(
        "stray",
        "running",
        &[(LABEL_WORKSPACE, "ws-1"), (LABEL_GENERATION, "01")],
    );
    assert_eq!(code(&h.ensure("ws-1", 1).await), LauncherErrorCode::Busy);
    assert_eq!(
        h.simple(Request::Inspect {
            workspace_id: "ws-1".into()
        })
        .await,
        Reply::Inspected {
            running: None,
            stopped_through: None
        }
    );
    h.fake.with(|s| s.sticky = true);
    let reply = h.stop("ws-1", 1).await;
    assert!(!proven(&reply), "{reply:?}");
    assert_eq!(h.fake.count(&format!("remove {stray}")), 1);
    h.fake.with(|s| s.sticky = false);
    assert!(proven(&h.stop("ws-1", 1).await));
    assert!(h.fake.with(|s| s.containers.is_empty()));
    ensured(h.ensure("ws-1", 2).await);
}

/// `stop(G)` proves every generation at or below `G` gone, not only `G`:
/// a leftover container of a lower generation (created and never started,
/// or still running) is stopped and removed too, while a higher
/// generation's container is left alone.
#[tokio::test]
async fn stop_covers_every_lower_generation_and_spares_higher_ones() {
    let h = Harness::new();
    let labels = |generation: &'static str| {
        [
            (LABEL_WORKSPACE, "ws-1"),
            (LABEL_GENERATION, generation),
            (LABEL_MANAGED, "1"),
        ]
    };
    let leftover = h.fake.insert("aks-ws-1-g1", "created", &labels("1"));
    let old = h.fake.insert("aks-ws-1-g2", "running", &labels("2"));
    let newer = h.fake.insert("aks-ws-1-g7", "running", &labels("7"));
    assert!(proven(&h.stop("ws-1", 3).await));
    let left: Vec<String> = h
        .fake
        .with(|s| s.containers.iter().map(|c| c.id.clone()).collect());
    assert_eq!(left, [newer], "generations 1 and 2 are gone, 7 stays");
    for removed in [&leftover, &old] {
        assert_eq!(h.fake.count(&format!("remove {removed}")), 1);
    }
    assert_eq!(code(&h.ensure("ws-1", 3).await), stale(7));
    assert_eq!(
        h.simple(Request::Inspect {
            workspace_id: "ws-1".into()
        })
        .await,
        Reply::Inspected {
            running: Some(7),
            stopped_through: Some(3)
        }
    );
    // A lower generation that stays listed keeps the stop unproven and
    // records nothing.
    let stuck = h.fake.insert("aks-ws-1-g4", "exited", &labels("4"));
    h.fake.with(|s| s.sticky = true);
    let reply = h.stop("ws-1", 5).await;
    assert!(!proven(&reply), "{reply:?}");
    assert!(h.fake.count(&format!("remove {stuck}")) >= 1);
    h.fake.with(|s| s.sticky = false);
    let inspected = h
        .simple(Request::Inspect {
            workspace_id: "ws-1".into(),
        })
        .await;
    assert_eq!(
        inspected,
        Reply::Inspected {
            running: Some(7),
            stopped_through: Some(3)
        }
    );
}

#[tokio::test]
async fn another_workspaces_containers_are_ignored_even_unfiltered() {
    let h = Harness::new();
    ensured(h.ensure("ws-2", 7).await);
    h.fake.with(|s| s.ignore_filter = true);
    ensured(h.ensure("ws-1", 1).await);
    assert!(proven(&h.stop("ws-1", 2).await));
}

/// `stopped` is answered only after the runtime stops listing it.
#[tokio::test]
async fn a_container_that_stays_listed_is_not_proven() {
    let h = Harness::new();
    ensured(h.ensure("ws-1", 1).await);
    h.fake.with(|s| s.sticky = true);
    let reply = h.stop("ws-1", 1).await;
    assert!(!proven(&reply), "{reply:?}");
    assert!(h.fake.count("list ") > 2, "the proof retried");
    assert!(h.machine_dir("ws-1", 1).exists());
    assert_eq!(code(&h.ensure("ws-1", 2).await), LauncherErrorCode::Busy);
    h.fake.with(|s| s.sticky = false);
    assert!(proven(&h.stop("ws-1", 1).await));
    ensured(h.ensure("ws-1", 2).await);
}

/// A generation that never started is proven by the runtime's empty
/// listing, and is then refused for good (the conformance suite's
/// `proven_generations_never_start`).
#[tokio::test]
async fn a_never_started_generation_is_proven_by_absence() {
    let h = Harness::new();
    ensured(h.ensure("ws-1", 1).await);
    assert!(proven(&h.stop("ws-1", 1).await));
    assert!(proven(&h.stop("ws-1", 3).await));
    assert_eq!(code(&h.ensure("ws-1", 2).await), stale(3));
    assert_eq!(code(&h.ensure("ws-1", 3).await), stale(3));
    ensured(h.ensure("ws-1", 4).await);
    h.fake.with(|s| s.down = true);
    let reply = h.stop("ws-1", 9).await;
    assert!(!proven(&reply), "{reply:?}");
    h.fake.with(|s| s.down = false);
    assert_eq!(code(&h.ensure("ws-1", 9).await), LauncherErrorCode::Busy);
}

#[tokio::test]
async fn stop_grace_is_rounded_up_and_capped() {
    let h = Harness::new();
    for (grace_ms, expected) in [(0, "t=0"), (1, "t=1"), (2_500, "t=3"), (600_000, "t=30")] {
        let generation = h.fake.count("create aks-") as u64 + 1;
        ensured(h.ensure("ws-1", generation).await);
        assert!(proven(
            &h.stop_with_grace("ws-1", generation, grace_ms).await
        ));
        let calls = h.fake.calls();
        let stop = calls.iter().rev().find(|c| c.starts_with("stop ")).unwrap();
        assert!(stop.ends_with(expected), "{grace_ms}: {stop}");
    }
}

#[tokio::test]
async fn a_warning_or_failed_start_removes_the_container() {
    let h = Harness::new();
    h.fake
        .with(|s| s.create_warnings = vec!["swap limit discarded".into()]);
    let reply = h.ensure("ws-1", 1).await;
    assert_eq!(code(&reply), LauncherErrorCode::RuntimeFailed);
    assert!(h.fake.with(|s| s.containers.is_empty()));
    h.fake.with(|s| s.create_warnings.clear());
    assert_eq!(
        code(&h.ensure("ws-1", 1).await),
        LauncherErrorCode::Busy,
        "a generation gets one container"
    );
    assert!(proven(&h.stop("ws-1", 1).await));
    h.fake.with(|s| s.fail_start = true);
    assert_eq!(
        code(&h.ensure("ws-1", 2).await),
        LauncherErrorCode::RuntimeFailed
    );
    assert!(h.fake.with(|s| s.containers.is_empty()));
    h.fake.with(|s| s.fail_start = false);
    assert!(proven(&h.stop("ws-1", 2).await));
    let id = ensured(h.ensure("ws-1", 3).await);
    assert_eq!(h.container_id("aks-ws-1-g3"), id);
    assert_eq!(h.fake.count("create aks-"), 3);
}

#[tokio::test]
async fn a_linked_socket_directory_is_refused_before_create() {
    let h = Harness::new();
    let outside = h.root.join("outside");
    std::fs::create_dir(&outside).unwrap();
    symlink(&outside, h.machine_dir("ws-1", 1)).unwrap();
    let reply = h.ensure("ws-1", 1).await;
    assert_eq!(code(&reply), LauncherErrorCode::RuntimeFailed);
    assert_eq!(h.fake.count("create"), 0);
    assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
    std::fs::remove_file(h.machine_dir("ws-1", 1)).unwrap();
    ensured(h.ensure("ws-1", 1).await);
}

/// The socket host directory is checked at every start: opened to others
/// after startup, it refuses the next create before anything is made.
#[tokio::test]
async fn a_socket_host_directory_opened_to_others_refuses_the_start() {
    use std::os::unix::fs::PermissionsExt;
    let h = Harness::new();
    let sockets = h.root.join("sockets");
    std::fs::set_permissions(&sockets, std::fs::Permissions::from_mode(0o755)).unwrap();
    let reply = h.ensure("ws-1", 1).await;
    assert_eq!(code(&reply), LauncherErrorCode::RuntimeFailed);
    assert_eq!(h.fake.count("create"), 0);
    assert!(!h.machine_dir("ws-1", 1).exists());
    std::fs::set_permissions(&sockets, std::fs::Permissions::from_mode(0o700)).unwrap();
    ensured(h.ensure("ws-1", 1).await);
}

/// The state directory is checked again at every start, like the socket
/// host directory: opened to others, it refuses the create before anything
/// is made or recorded.
#[tokio::test]
async fn a_state_directory_opened_to_others_refuses_the_start() {
    use std::os::unix::fs::PermissionsExt;
    let h = Harness::new();
    let state = h.root.join("state");
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o777)).unwrap();
    let reply = h.ensure("ws-1", 1).await;
    assert_eq!(code(&reply), LauncherErrorCode::RuntimeFailed);
    match &reply {
        Reply::Error(error) => assert!(error.message.contains("state directory"), "{error:?}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(h.fake.count("create"), 0);
    assert!(!h.machine_dir("ws-1", 1).exists());
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o755)).unwrap();
    ensured(h.ensure("ws-1", 1).await);
}

/// One machine directory per workspace at most is left behind: a stop
/// removes those of the generations it covers, a create the one of the
/// generation created before, and `destroy` the rest.
#[tokio::test]
async fn machine_directories_do_not_pile_up() {
    let h = Harness::new();
    let id = ensured(h.ensure("ws-1", 1).await);
    // The container vanishes outside the launcher; no stop ever covers g1.
    h.fake.with(|s| s.containers.retain(|c| c.id != id));
    assert!(h.machine_dir("ws-1", 1).exists());
    ensured(h.ensure("ws-1", 2).await);
    assert!(
        !h.machine_dir("ws-1", 1).exists(),
        "removed by the next create"
    );
    assert!(h.machine_dir("ws-1", 2).exists());
    assert!(proven(&h.stop("ws-1", 2).await));
    assert!(!h.machine_dir("ws-1", 2).exists());
    let id = ensured(h.ensure("ws-1", 3).await);
    h.fake.with(|s| s.containers.retain(|c| c.id != id));
    assert_eq!(
        h.simple(Request::Destroy {
            workspace_id: "ws-1".into()
        })
        .await,
        Reply::Done
    );
    assert_eq!(
        std::fs::read_dir(h.root.join("sockets")).unwrap().count(),
        0
    );
}

/// `max_containers` counts every managed container, across workspaces and
/// in any state; a refused create records nothing, so the generation can
/// start once there is room. Concurrent creates never overshoot.
#[tokio::test]
async fn the_container_cap_refuses_creates_without_burning_the_generation() {
    let mut h = Harness::new();
    h.reconfigure(|config| config.max_containers = 2);
    let first = ensured(h.ensure("ws-1", 1).await);
    h.fake.set_state(&first, "exited");
    ensured(h.ensure("ws-2", 1).await);
    let refused = h.ensure("ws-3", 1).await;
    assert_eq!(code(&refused), LauncherErrorCode::RuntimeFailed);
    match &refused {
        Reply::Error(error) => assert!(error.message.contains("maximum of 2"), "{error:?}"),
        other => panic!("{other:?}"),
    }
    assert!(!h.machine_dir("ws-3", 1).exists());
    assert_eq!(h.fake.count("create aks-ws-3"), 0);
    assert_eq!(h.fake.count("create_volume aks-ws-ws-3"), 0);
    // An existing machine is still answered at the cap.
    assert!(matches!(h.ensure("ws-2", 1).await, Reply::Ensured { .. }));
    assert!(proven(&h.stop("ws-1", 1).await));
    ensured(h.ensure("ws-3", 1).await);

    let mut h = Harness::new();
    h.reconfigure(|config| config.max_containers = 2);
    let replies = three_workspaces_at_once(&h).await;
    let started = replies
        .iter()
        .filter(|r| matches!(r, Reply::Ensured { .. }))
        .count();
    assert_eq!(started, 2, "{replies:?}");
    assert_eq!(h.fake.count("create aks-"), 2);
}

async fn three_workspaces_at_once(h: &Harness) -> [Reply; 3] {
    let (a, b, c) = tokio::join!(
        h.ensure("ws-a", 1),
        h.ensure("ws-b", 1),
        h.ensure("ws-c", 1)
    );
    [a, b, c]
}

#[tokio::test]
async fn a_foreign_volume_of_the_same_name_is_refused() {
    let h = Harness::new();
    let bind = VolumeInfo {
        driver: "local".into(),
        labels: BTreeMap::new(),
        options: BTreeMap::from([
            ("type".to_string(), "none".to_string()),
            ("o".to_string(), "bind".to_string()),
            ("device".to_string(), "/etc".to_string()),
        ]),
    };
    h.fake
        .with(|s| s.volumes.insert("aks-ws-ws-1".into(), bind));
    let reply = h.ensure("ws-1", 1).await;
    assert_eq!(code(&reply), LauncherErrorCode::RuntimeFailed);
    assert_eq!(h.fake.count("create aks-"), 0);
}
