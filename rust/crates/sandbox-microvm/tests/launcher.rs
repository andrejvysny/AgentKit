//! `VmLauncher` over fake helpers: the launcher protocol, generation rules,
//! stop proofs, capacity, failed starts and image checks.

mod support;

use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};

use agentkit_sandbox_microvm::helper::{HelperConfig, kernel_cmdline};
use agentkit_sandbox_microvm::{VmLauncher, VmStartupError};
use agentkit_sandbox_proto::launcher::LauncherErrorCode;
use agentkit_sandbox_proto::machine_dir_name;
use support::*;

#[tokio::test]
async fn ensure_answers_the_running_generation_with_its_token() {
    let fixture = Fixture::new(NORMAL);
    let launcher = fixture.start().await;
    let digest = launcher.image_digest().to_owned();
    assert_eq!(digest, fixture.config.image_digest());
    let (id, token) = ensured(call(&launcher, ensure("ws-a", 1, &digest)).await);
    assert!(token.len() == 32 && token.bytes().all(|b| b.is_ascii_hexdigit()));
    let again = ensured(call(&launcher, ensure("ws-a", 1, &digest)).await);
    assert_eq!(again, (id.clone(), token.clone()), "idempotent");
    assert_eq!(
        inspected(call(&launcher, inspect("ws-a")).await),
        (Some(1), None)
    );
    let seen: HelperConfig = serde_json::from_str(fixture.seen().trim()).unwrap();
    assert_eq!((seen.cpus, seen.memory_bytes), (2, 2 << 30));
    assert_eq!(seen.cmdline, kernel_cmdline(1, &digest, &token));
    let ws_dir = fixture.workspace_dir("ws-a");
    assert_eq!(
        seen.workspace_disk,
        ws_dir.join("workspace.img").to_str().unwrap()
    );
    assert_eq!(seen.home_disk, ws_dir.join("home.img").to_str().unwrap());
    let machine_dir = launcher.socket_root().join(machine_dir_name("ws-a", 1));
    assert_eq!(
        seen.control_socket,
        machine_dir.join("control.sock").to_str().unwrap()
    );
    let mode = std::fs::metadata(&machine_dir)
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o700);
    let state = std::fs::read_to_string(fixture.config.data_root.join("state/machines.json"));
    assert!(
        !state.unwrap().contains(&token),
        "the token is never persisted"
    );
    launcher.shutdown().await.unwrap();
}

#[tokio::test]
async fn generations_follow_the_launcher_rules() {
    let fixture = Fixture::new(NORMAL);
    let launcher = fixture.start().await;
    let digest = launcher.image_digest().to_owned();
    let invalid = LauncherErrorCode::InvalidRequest;
    assert_eq!(
        refusal(call(&launcher, ensure("ws", 0, &digest)).await),
        invalid
    );
    assert_eq!(
        refusal(call(&launcher, ensure("WS", 1, &digest)).await),
        invalid
    );
    let other = ensure("ws", 1, "sha256:other");
    assert_eq!(
        refusal(call(&launcher, other).await),
        LauncherErrorCode::ImageMismatch
    );
    let mut odd_tier = ensure("ws", 1, &digest);
    if let agentkit_sandbox_proto::launcher::Request::Ensure { tier, .. } = &mut odd_tier {
        tier.pids += 1;
    }
    assert_eq!(refusal(call(&launcher, odd_tier).await), invalid);
    ensured(call(&launcher, ensure("ws", 2, &digest)).await);
    let stale = LauncherErrorCode::StaleGeneration { current: 2 };
    assert_eq!(
        refusal(call(&launcher, ensure("ws", 1, &digest)).await),
        stale
    );
    let busy = LauncherErrorCode::Busy;
    assert_eq!(
        refusal(call(&launcher, ensure("ws", 3, &digest)).await),
        busy
    );
    assert!(proven(call(&launcher, stop("ws", 2, 5_000)).await));
    assert_eq!(
        refusal(call(&launcher, ensure("ws", 2, &digest)).await),
        stale
    );
    ensured(call(&launcher, ensure("ws", 3, &digest)).await);
    assert_eq!(
        inspected(call(&launcher, inspect("ws")).await),
        (Some(3), Some(2))
    );
    launcher.shutdown().await.unwrap();
}

#[tokio::test]
async fn stop_is_proven_once_the_helper_is_reaped_even_when_it_ignores_eof() {
    let fixture = Fixture::new(STUBBORN);
    let launcher = fixture.start().await;
    let digest = launcher.image_digest().to_owned();
    let (id, _) = ensured(call(&launcher, ensure("ws", 1, &digest)).await);
    let pid = pid_of(&id);
    assert!(agentkit_sys::pid_alive(pid));
    let begun = Instant::now();
    assert!(proven(call(&launcher, stop("ws", 1, 300)).await));
    assert!(begun.elapsed() < Duration::from_secs(10));
    assert!(
        !agentkit_sys::pid_alive(pid),
        "the helper was SIGKILLed and reaped"
    );
    assert_eq!(
        inspected(call(&launcher, inspect("ws")).await),
        (None, Some(1))
    );
    let machine_dir = launcher.socket_root().join(machine_dir_name("ws", 1));
    assert!(
        !machine_dir.exists(),
        "the stop removed the machine directory"
    );
    assert!(
        proven(call(&launcher, stop("ws", 1, 300)).await),
        "nothing held"
    );
    launcher.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_helper_that_never_connects_fails_the_ensure_and_burns_the_generation() {
    let mut fixture = Fixture::new(SILENT);
    fixture.config.connect_timeout = Duration::from_secs(1);
    let launcher = fixture.start().await;
    let digest = launcher.image_digest().to_owned();
    let begun = Instant::now();
    let failed = call(&launcher, ensure("ws", 1, &digest)).await;
    assert!(begun.elapsed() >= Duration::from_secs(1));
    assert_eq!(refusal(failed), LauncherErrorCode::RuntimeFailed);
    assert_eq!(
        inspected(call(&launcher, inspect("ws")).await),
        (None, None)
    );
    let busy = LauncherErrorCode::Busy;
    assert_eq!(
        refusal(call(&launcher, ensure("ws", 1, &digest)).await),
        busy
    );
    assert!(proven(call(&launcher, stop("ws", 1, 0)).await));
    let stale = LauncherErrorCode::StaleGeneration { current: 1 };
    assert_eq!(
        refusal(call(&launcher, ensure("ws", 1, &digest)).await),
        stale
    );
    launcher.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_helper_that_stops_before_connecting_is_reported() {
    let fixture = Fixture::new(EARLY_EXIT);
    let launcher = fixture.start().await;
    let digest = launcher.image_digest().to_owned();
    match call(&launcher, ensure("ws", 1, &digest)).await {
        agentkit_sandbox_proto::launcher::Reply::Error(error) => {
            assert_eq!(error.code, LauncherErrorCode::RuntimeFailed);
            assert!(error.message.contains("boom"), "{}", error.message);
        }
        other => panic!("expected a failure, got {other:?}"),
    }
    assert_eq!(
        inspected(call(&launcher, inspect("ws")).await),
        (None, None)
    );
    launcher.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_helper_that_dies_on_its_own_stays_held_until_a_stop() {
    let mut fixture = Fixture::new(DIES);
    fixture.config.max_machines = 1;
    let launcher = fixture.start().await;
    let digest = launcher.image_digest().to_owned();
    ensured(call(&launcher, ensure("ws-a", 1, &digest)).await);
    let deadline = Instant::now() + Duration::from_secs(5);
    while inspected(call(&launcher, inspect("ws-a")).await)
        .0
        .is_some()
    {
        assert!(
            Instant::now() < deadline,
            "the dead helper still reads as running"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let busy = LauncherErrorCode::Busy;
    assert_eq!(
        refusal(call(&launcher, ensure("ws-a", 1, &digest)).await),
        busy
    );
    let reset = agentkit_sandbox_proto::launcher::Request::ResetHome {
        workspace_id: "ws-a".into(),
    };
    assert_eq!(refusal(call(&launcher, reset).await), busy, "still held");
    ensured(call(&launcher, ensure("ws-b", 1, &digest)).await);
    assert!(proven(call(&launcher, stop("ws-a", 1, 1_000)).await));
    assert_eq!(
        inspected(call(&launcher, inspect("ws-a")).await),
        (None, Some(1))
    );
    launcher.shutdown().await.unwrap();
}

#[tokio::test]
async fn max_machines_is_a_transient_refusal_that_records_nothing() {
    let mut fixture = Fixture::new(NORMAL);
    fixture.config.max_machines = 1;
    let launcher = fixture.start().await;
    let digest = launcher.image_digest().to_owned();
    ensured(call(&launcher, ensure("ws-a", 1, &digest)).await);
    let full = call(&launcher, ensure("ws-b", 1, &digest)).await;
    assert_eq!(refusal(full), LauncherErrorCode::RuntimeFailed);
    assert!(proven(call(&launcher, stop("ws-a", 1, 5_000)).await));
    ensured(call(&launcher, ensure("ws-b", 1, &digest)).await);
    launcher.shutdown().await.unwrap();
}

#[tokio::test]
async fn images_are_verified_at_startup_and_before_each_start() {
    let mut fixture = Fixture::new(NORMAL);
    let configured = fixture.config.root_disk.sha256.clone();
    fixture.config.root_disk.sha256 = "0".repeat(64);
    let refused = VmLauncher::start(fixture.config.clone()).await.unwrap_err();
    assert!(matches!(refused, VmStartupError::Image(_)), "{refused}");
    fixture.config.root_disk.sha256 = configured;
    let launcher = fixture.start().await;
    let digest = launcher.image_digest().to_owned();
    let root = &fixture.config.root_disk.path;
    std::fs::remove_file(root).unwrap();
    std::fs::write(root, b"another root disk").unwrap();
    let changed = call(&launcher, ensure("ws", 1, &digest)).await;
    assert_eq!(refusal(changed), LauncherErrorCode::ImageMismatch);
    launcher.shutdown().await.unwrap();
}

#[tokio::test]
async fn startup_refuses_bad_configs_and_a_second_launcher() {
    let mut fixture = Fixture::new(NORMAL);
    let socket_root = fixture.config.socket_root.clone();
    fixture.config.socket_root = socket_root.join("d".repeat(80));
    let long = VmLauncher::start(fixture.config.clone()).await.unwrap_err();
    assert!(matches!(long, VmStartupError::Config(_)), "{long}");
    fixture.config.socket_root = socket_root;
    let first = fixture.start().await;
    let other_sockets = socket_dir();
    let mut same_data = fixture.config.clone();
    same_data.socket_root = other_sockets.path().to_path_buf();
    let second = VmLauncher::start(same_data).await.unwrap_err();
    assert!(matches!(second, VmStartupError::State(_)), "{second}");
    let mut same_sockets = fixture.config.clone();
    same_sockets.data_root = fixture.scratch.path().join("other-data");
    let third = VmLauncher::start(same_sockets).await.unwrap_err();
    assert!(matches!(third, VmStartupError::Directory(_)), "{third}");
    let reply = call(&first, inspect("ws")).await;
    assert_eq!(inspected(reply), (None, None), "the first still serves");
    first.shutdown().await.unwrap();
}
