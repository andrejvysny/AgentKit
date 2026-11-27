//! Test support: fake helpers (shell scripts speaking the helper's stdin
//! and stdout contract), scratch roots and images, and a launcher client.

#![allow(dead_code)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use agentkit_sandbox_microvm::{ImageFile, VmLauncher, VmLauncherConfig};
use agentkit_sandbox_proto::launcher::{LauncherErrorCode, Reply, Request, Tier};
use agentkit_sandbox_proto::{Envelope, Frame, FrameDecoder, encode_control};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

/// Reads its configuration line into `seen.json` next to itself, reports
/// `started` and `connected`, waits for EOF on stdin, reports `stopped`.
/// A workspace named `ws-stay` ignores EOF instead (it outlives its
/// launcher and is only ended by SIGKILL).
pub const NORMAL: &str = r#"IFS= read -r line || exit 2
printf '%s\n' "$line" > "$(dirname "$0")/seen.json"
echo '{"event":"started"}'
echo '{"event":"connected"}'
case "$line" in
  */ws-stay/*) exec sleep 600 ;;
esac
cat > /dev/null
echo '{"event":"stopped","reason":"stdin closed"}'
"#;
/// Connects, then ignores EOF: only SIGKILL ends it.
pub const STUBBORN: &str = r#"IFS= read -r line || exit 2
echo '{"event":"started"}'
echo '{"event":"connected"}'
exec sleep 600
"#;
/// Starts but never connects.
pub const SILENT: &str = r#"IFS= read -r line || exit 2
echo '{"event":"started"}'
exec sleep 600
"#;
/// Connects, then dies on its own (a crashed VM).
pub const DIES: &str = r#"IFS= read -r line || exit 2
echo '{"event":"started"}'
echo '{"event":"connected"}'
exit 1
"#;
/// Fails before it connects.
pub const EARLY_EXIT: &str = r#"IFS= read -r line || exit 2
echo '{"event":"started"}'
echo '{"event":"stopped","reason":"boom: the kernel is missing"}'
exit 2
"#;

/// Scratch roots, fake images and a fake helper.
pub struct Fixture {
    pub scratch: TempDir,
    /// Short, so the socket paths fit `sun_path`.
    pub sockets: TempDir,
    pub config: VmLauncherConfig,
}

impl Fixture {
    /// A launcher configuration over the fake helper `body`.
    pub fn new(body: &str) -> Self {
        let scratch = tempfile::tempdir().unwrap();
        let sockets = socket_dir();
        let helper = write_script(scratch.path(), "helper.sh", body);
        let kernel = image(scratch.path(), "Image", b"kernel");
        let root = image(scratch.path(), "root.ext4", b"root disk");
        let mut config = VmLauncherConfig::new(
            scratch.path().join("data"),
            sockets.path().to_path_buf(),
            helper,
            kernel,
            root,
        );
        config.connect_timeout = Duration::from_secs(5);
        Self {
            scratch,
            sockets,
            config,
        }
    }

    pub async fn start(&self) -> VmLauncher {
        VmLauncher::start(self.config.clone()).await.unwrap()
    }

    /// The configuration line the last NORMAL helper read.
    pub fn seen(&self) -> String {
        std::fs::read_to_string(self.scratch.path().join("seen.json")).unwrap()
    }

    /// The files of workspace `ws` below the data root.
    pub fn workspace_dir(&self, ws: &str) -> PathBuf {
        self.config.data_root.join("workspaces").join(ws)
    }
}

/// A short private directory for a socket root (`/tmp` itself is
/// world-writable; the launcher refuses a root that is not 0700).
pub fn socket_dir() -> TempDir {
    tempfile::Builder::new()
        .prefix("aksvm")
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir_in("/tmp")
        .unwrap()
}

fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    path
}

fn image(dir: &Path, name: &str, bytes: &[u8]) -> ImageFile {
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap();
    ImageFile {
        path,
        sha256: sha256_hex(bytes),
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn tier() -> Tier {
    Tier {
        cpus_milli: 2000,
        memory_bytes: 2 << 30,
        pids: 512,
    }
}

pub fn ensure(ws: &str, generation: u64, image_digest: &str) -> Request {
    Request::Ensure {
        workspace_id: ws.to_owned(),
        generation,
        image_digest: image_digest.to_owned(),
        tier: tier(),
    }
}

pub fn stop(ws: &str, generation: u64, grace_ms: u64) -> Request {
    Request::Stop {
        workspace_id: ws.to_owned(),
        generation,
        grace_ms,
    }
}

pub fn inspect(ws: &str) -> Request {
    Request::Inspect {
        workspace_id: ws.to_owned(),
    }
}

/// One request on a fresh connection, as the adapter sends them.
pub async fn call(launcher: &VmLauncher, request: Request) -> Reply {
    let mut stream = UnixStream::connect(launcher.listen_socket()).await.unwrap();
    let frame = encode_control(&Envelope::new(7, &request)).unwrap();
    stream.write_all(&frame).await.unwrap();
    let mut decoder = FrameDecoder::new();
    let mut buf = vec![0_u8; 64 * 1024];
    loop {
        if let Some(Frame::Control(json)) = decoder.next_frame().unwrap() {
            let envelope: Envelope<Reply> = serde_json::from_str(&json).unwrap();
            assert_eq!(envelope.id, 7);
            return envelope.body;
        }
        let read = stream.read(&mut buf).await.unwrap();
        assert!(read > 0, "the launcher closed without a reply");
        decoder.push(&buf[..read]);
    }
}

/// The machine id and token of an `ensured` reply.
pub fn ensured(reply: Reply) -> (String, String) {
    match reply {
        Reply::Ensured {
            container_id,
            channel_token,
        } => (container_id, channel_token),
        other => panic!("expected ensured, got {other:?}"),
    }
}

/// The helper pid in a machine id `vm-g<generation>-p<pid>`.
pub fn pid_of(machine_id: &str) -> u32 {
    machine_id.rsplit_once("-p").unwrap().1.parse().unwrap()
}

/// The error code of a refusal.
pub fn refusal(reply: Reply) -> LauncherErrorCode {
    match reply {
        Reply::Error(error) => error.code,
        other => panic!("expected an error, got {other:?}"),
    }
}

pub fn proven(reply: Reply) -> bool {
    match reply {
        Reply::Stopped { proven, .. } => proven,
        other => panic!("expected stopped, got {other:?}"),
    }
}

/// `running` and `stopped_through` of an `inspected` reply.
pub fn inspected(reply: Reply) -> (Option<u64>, Option<u64>) {
    match reply {
        Reply::Inspected {
            running,
            stopped_through,
        } => (running, stopped_through),
        other => panic!("expected inspected, got {other:?}"),
    }
}
