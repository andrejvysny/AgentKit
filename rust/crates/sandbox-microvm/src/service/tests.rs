use std::path::Path;

use agentkit_sandbox_proto::launcher::{LauncherErrorCode, Tier};

use super::*;
use crate::config::ImageFile;
use crate::machine::Exited;

/// SHA-256 of the three bytes `abc`.
const ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

/// A service over scratch files whose disk release wait is short.
fn service(scratch: &Path) -> Service {
    let uid = agentkit_sys::effective_uid().unwrap();
    let image = |name: &str| {
        let path = scratch.join(name);
        std::fs::write(&path, b"abc").unwrap();
        ImageFile {
            path,
            sha256: ABC.to_owned(),
        }
    };
    let config = VmLauncherConfig::new(
        scratch.join("data"),
        scratch.join("sockets"),
        scratch.join("helper"),
        image("Image"),
        image("root.ext4"),
    );
    let images = Images::verify(&config.kernel, &config.root_disk).unwrap();
    let state_dir = scratch.join("state");
    std::fs::create_dir(&state_dir).unwrap();
    let state = StateStore::open(&state_dir).unwrap();
    let (workspace_bytes, home_bytes) = (config.workspace_disk_bytes, config.home_disk_bytes);
    let disks = Disks::new(scratch.join("workspaces"), uid, workspace_bytes, home_bytes);
    let lock = std::fs::File::create(scratch.join("socket.lock")).unwrap();
    let mut service = Service::new(Arc::new(config), images, state, disks, lock, uid);
    service.release_wait = Duration::from_millis(200);
    service
}

fn stop(ws: &str) -> Request {
    Request::Stop {
        workspace_id: ws.to_owned(),
        generation: 1,
        grace_ms: 0,
    }
}

fn ensure(ws: &str, image_digest: &str) -> Request {
    Request::Ensure {
        workspace_id: ws.to_owned(),
        generation: 1,
        image_digest: image_digest.to_owned(),
        tier: Tier {
            cpus_milli: 2000,
            memory_bytes: 2 << 30,
            pids: 512,
        },
    }
}

#[tokio::test]
async fn held_disks_refuse_a_stop_proof_and_a_start() {
    let scratch = tempfile::tempdir().unwrap();
    let service = service(scratch.path());
    let ws = WorkspaceId::parse("ws").unwrap();
    let files = service.disks.prepare(&ws).unwrap();
    // A VM that outlived its helper, played by this process.
    let held = std::fs::File::open(&files.workspace_disk).unwrap();
    match service.handle(stop("ws")).await {
        Reply::Stopped {
            proven: false,
            reason: Some(reason),
        } => assert!(reason.contains("still open"), "{reason}"),
        other => panic!("expected an unproven stop, got {other:?}"),
    }
    assert_eq!(service.state.get(&ws).await.stopped_through, 0);
    let digest = service.image_digest.clone();
    match service.handle(ensure("ws", &digest)).await {
        Reply::Error(error) => {
            assert_eq!(error.code, LauncherErrorCode::RuntimeFailed);
            assert!(error.message.contains("still open"), "{}", error.message);
        }
        other => panic!("expected a refused start, got {other:?}"),
    }
    let record = service.state.get(&ws).await;
    assert_eq!(record.created_through, 0, "a refused start records nothing");
    drop(held);
    let proven = service.handle(stop("ws")).await;
    assert!(
        matches!(proven, Reply::Stopped { proven: true, .. }),
        "{proven:?}"
    );
}

#[tokio::test]
async fn a_shutdown_refuses_requests_and_holds_no_new_machine() {
    let scratch = tempfile::tempdir().unwrap();
    let service = service(scratch.path());
    let ws = WorkspaceId::parse("ws").unwrap();
    let log = std::fs::File::create(scratch.path().join("helper.log")).unwrap();
    // `cat` speaks no event, but it ends at EOF on stdin like a helper.
    let machine = Arc::new(Machine::spawn(Path::new("/bin/cat"), log, 1, "t".into()).unwrap());
    assert!(service.close_and_held().is_empty());
    assert!(!service.insert(&ws, Arc::clone(&machine)), "refused");
    assert!(service.machine(&ws).is_none());
    let inspect = Request::Inspect {
        workspace_id: "ws".into(),
    };
    match service.handle(inspect).await {
        Reply::Error(error) => {
            assert_eq!(error.code, LauncherErrorCode::RuntimeFailed);
            assert_eq!(error.message, "the launcher is shutting down");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    match machine.stop(Duration::from_secs(5)).await.unwrap() {
        Exited::Status(status) => assert!(status.success(), "{status}"),
        Exited::Unknown => panic!("the waiter reaps its own child"),
    }
}

#[tokio::test]
async fn a_stop_below_a_held_newer_machine_leaves_its_disks_to_it() {
    let scratch = tempfile::tempdir().unwrap();
    let service = service(scratch.path());
    let ws = WorkspaceId::parse("ws").unwrap();
    let files = service.disks.prepare(&ws).unwrap();
    let log = std::fs::File::create(scratch.path().join("helper.log")).unwrap();
    let newer = Arc::new(Machine::spawn(Path::new("/bin/cat"), log, 2, "t".into()).unwrap());
    assert!(service.insert(&ws, Arc::clone(&newer)));
    // The disks belong to the running generation 2.
    let held = std::fs::File::open(&files.workspace_disk).unwrap();
    let reply = service.handle(stop("ws")).await;
    assert!(
        matches!(reply, Reply::Stopped { proven: true, .. }),
        "{reply:?}"
    );
    assert!(newer.running(), "the newer machine is left alone");
    assert_eq!(service.state.get(&ws).await.stopped_through, 1);
    drop(held);
    newer.stop(Duration::from_secs(5)).await.unwrap();
}
