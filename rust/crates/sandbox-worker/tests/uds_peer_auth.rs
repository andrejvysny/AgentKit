//! UDS peer-credential admission: allowed uid gets service, everyone else
//! is closed silently (no reply), with the socket at 0600 in a 0700 dir.

mod support;

use agentkit_sandbox_contracts::api::messages::{HostToWorker, WorkerToHost};
use agentkit_sandbox_contracts::protocol::{FrameDecoder, FrameHeader, encode_frame};
use agentkit_sandbox_worker::transport::{bind_uds, serve};
use support::Harness;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

fn hello_frame() -> Vec<u8> {
    let header = FrameHeader {
        protocol_version: agentkit_sandbox_contracts::PROTOCOL_VERSION,
        request_id: "hello-1".into(),
        sandbox_id: None,
        instance_generation: None,
        execution_id: None,
        operation: "hello".into(),
        sequence: 1,
        deadline_ms: None,
        payload_len: 0,
        payload_digest: None,
    };
    encode_frame(
        header,
        &HostToWorker::Hello {
            host_id: "test-host".into(),
            protocol_versions: vec![1],
        },
        false,
    )
    .unwrap()
}

async fn read_one_reply(stream: &mut UnixStream) -> Option<WorkerToHost> {
    let mut decoder = FrameDecoder::new();
    let mut buf = [0u8; 8192];
    for _ in 0..50 {
        while let Some(frame) = decoder.next_frame(u64::MAX).ok()? {
            if let Ok(msg) = serde_json::from_value::<WorkerToHost>(frame.body) {
                return Some(msg);
            }
        }
        let n = stream.read(&mut buf).await.ok()?;
        if n == 0 {
            return None;
        }
        decoder.push(&buf[..n]);
    }
    None
}

#[tokio::test]
async fn allowed_peer_uid_is_served() {
    let h = Harness::new();
    let uds = h.tmp.path().join("run/allowed.sock");
    let listener = bind_uds(&uds).unwrap();
    // Socket permissions: 0600 in a 0700 directory.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let dir_mode = std::fs::metadata(uds.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        let sock_mode = std::fs::metadata(&uds).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700, "uds dir must be 0700");
        assert_eq!(sock_mode, 0o600, "uds socket must be 0600");
    }
    let (tx, rx) = tokio::sync::broadcast::channel::<()>(1);
    let svc = std::sync::Arc::clone(&h.svc);
    let my_uid = unsafe { libc::getuid() };
    let handle = tokio::spawn(async move {
        serve(listener, svc, vec![my_uid], rx).await;
    });

    let mut client = UnixStream::connect(&uds).await.unwrap();
    client.write_all(&hello_frame()).await.unwrap();
    let reply = read_one_reply(&mut client).await.expect("reply expected");
    match reply {
        WorkerToHost::HelloAck {
            protocol_version,
            identity,
            capabilities,
        } => {
            assert_eq!(
                protocol_version,
                agentkit_sandbox_contracts::PROTOCOL_VERSION
            );
            assert_eq!(identity.worker_id.as_str(), "test-worker");
            assert!(capabilities.hardware_virtualization, "fake probes hw virt");
        }
        other => panic!("expected hello ack, got {other:?}"),
    }
    let _ = tx.send(());
    handle.await.unwrap();
}

#[tokio::test]
async fn peer_outside_allow_set_is_closed_silently() {
    let h = Harness::new();
    let uds = h.tmp.path().join("run/denied.sock");
    let listener = bind_uds(&uds).unwrap();
    let (_tx, rx) = tokio::sync::broadcast::channel::<()>(1);
    let svc = std::sync::Arc::clone(&h.svc);
    // Our uid is NOT in the allow set (offset keeps it impossible).
    let foreign = unsafe { libc::getuid() }.wrapping_add(12345);
    let handle = tokio::spawn(async move {
        serve(listener, svc, vec![foreign], rx).await;
    });

    let mut client = UnixStream::connect(&uds).await.unwrap();
    client.write_all(&hello_frame()).await.unwrap();
    client.flush().await.unwrap();
    // Silent close: the server checks the peer uid and closes without any
    // reply bytes. Both a clean EOF and an abrupt reset prove the close;
    // what must NEVER happen is receiving a protocol reply or hanging.
    let mut buf = [0u8; 128];
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(3), client.read(&mut buf))
        .await
        .expect("connection must be closed, not hang");
    match outcome {
        Ok(0) => {}
        Ok(n) => panic!("{n} reply bytes leaked to an unallowed peer"),
        // Reset with no data readable is equally a silent close.
        Err(_) => {
            let mut probe = [0u8; 4];
            let again = client.read(&mut probe).await.unwrap_or(0);
            assert_eq!(again, 0, "no bytes may be returned to an unallowed peer");
        }
    }
    handle.abort();
}
