//! `VmLauncher` disks over a fake helper: sparse creation, allocated-byte
//! usage, and `reset_home` and `destroy` refused while a machine is held.

mod support;

use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};

use agentkit_sandbox_proto::launcher::{LauncherErrorCode, Reply, Request};
use support::*;

const MIB: u64 = 1024 * 1024;

fn usage(ws: &str) -> Request {
    Request::Usage {
        workspace_id: ws.to_owned(),
    }
}

fn bytes(reply: Reply) -> (u64, u64) {
    match reply {
        Reply::Usage {
            workspace_bytes,
            home_bytes,
        } => (workspace_bytes, home_bytes),
        other => panic!("expected usage, got {other:?}"),
    }
}

fn write_at(path: &std::path::Path, offset: u64, len: u64) {
    let mut file = OpenOptions::new().write(true).open(path).unwrap();
    file.seek(SeekFrom::Start(offset)).unwrap();
    file.write_all(&vec![0x5a_u8; len as usize]).unwrap();
    file.sync_all().unwrap();
}

#[tokio::test]
async fn disks_are_sparse_and_usage_counts_allocated_bytes() {
    let fixture = Fixture::new(NORMAL);
    let launcher = fixture.start().await;
    let digest = launcher.image_digest().to_owned();
    assert_eq!(
        bytes(call(&launcher, usage("ws")).await),
        (0, 0),
        "no disks yet"
    );
    ensured(call(&launcher, ensure("ws", 1, &digest)).await);
    let disk = fixture.workspace_dir("ws").join("workspace.img");
    let size = std::fs::metadata(&disk).unwrap().len();
    assert_eq!(size, fixture.config.workspace_disk_bytes);
    let (workspace, home) = bytes(call(&launcher, usage("ws")).await);
    assert!(workspace < MIB && home < MIB, "sparse: {workspace} {home}");
    write_at(&disk, 512 * MIB, 2 * MIB);
    let (grown, _) = bytes(call(&launcher, usage("ws")).await);
    assert!(grown >= workspace + 2 * MIB, "{workspace} -> {grown}");
    launcher.shutdown().await.unwrap();
}

#[tokio::test]
async fn reset_home_and_destroy_wait_for_a_proven_stop() {
    let fixture = Fixture::new(NORMAL);
    let launcher = fixture.start().await;
    let digest = launcher.image_digest().to_owned();
    ensured(call(&launcher, ensure("ws", 1, &digest)).await);
    let reset = Request::ResetHome {
        workspace_id: "ws".into(),
    };
    let destroy = Request::Destroy {
        workspace_id: "ws".into(),
    };
    let busy = LauncherErrorCode::Busy;
    assert_eq!(refusal(call(&launcher, reset.clone()).await), busy);
    assert_eq!(refusal(call(&launcher, destroy.clone()).await), busy);
    assert!(proven(call(&launcher, stop("ws", 1, 5_000)).await));
    let home = fixture.workspace_dir("ws").join("home.img");
    write_at(&home, 64 * MIB, MIB);
    assert!(bytes(call(&launcher, usage("ws")).await).1 >= MIB);
    assert_eq!(call(&launcher, reset).await, Reply::Done);
    assert!(
        bytes(call(&launcher, usage("ws")).await).1 < MIB,
        "a fresh home"
    );
    let workspace = fixture.workspace_dir("ws").join("workspace.img");
    assert!(workspace.exists(), "reset_home keeps the workspace disk");
    assert_eq!(call(&launcher, destroy).await, Reply::Done);
    assert!(!fixture.workspace_dir("ws").exists());
    assert_eq!(bytes(call(&launcher, usage("ws")).await), (0, 0));
    let stale = LauncherErrorCode::StaleGeneration { current: 1 };
    let again = call(&launcher, ensure("ws", 1, &digest)).await;
    assert_eq!(refusal(again), stale, "destroy keeps the generation record");
    launcher.shutdown().await.unwrap();
}
