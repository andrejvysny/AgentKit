//! Firecracker vsock host-side dialer.
//!
//! Firecracker exposes a guest's vsock over a Unix socket using a tiny
//! text handshake: the client sends `CONNECT <port>\n` and the hypervisor
//! replies `OK <hash>\n`, after which the stream carries raw vsock bytes
//! (the guest agent's frame protocol). No other endpoint format exists;
//! anything else is refused (S16: no ad-hoc delimiter protocols beyond the
//! hypervisor's own fixed handshake).

use std::path::Path;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use crate::FcError;

/// Reply cap for the handshake line (Firecracker sends `OK <u64-hex>`).
const MAX_HANDSHAKE_LINE: usize = 128;

/// Dial the guest agent port through the VM's vsock UDS.
///
/// Returns the live stream plus any bytes the handshake reader observed
/// after the `OK` line (always empty with byte-exact reading; the tuple
/// keeps the API honest for future hypervisors).
pub async fn dial_guest_vsock_buffered(
    api_uds: &Path,
    port: u32,
    timeout: Duration,
) -> Result<(UnixStream, Vec<u8>), FcError> {
    let fut = async {
        let mut stream = UnixStream::connect(api_uds)
            .await
            .map_err(|e| FcError::io("vsock uds connect", e))?;
        stream
            .write_all(format!("CONNECT {port}\n").as_bytes())
            .await
            .map_err(|e| FcError::io("vsock connect line", e))?;
        // Read the reply byte-by-byte until '\n' so we never over-read.
        let mut line = Vec::new();
        let mut b = [0u8; 1];
        loop {
            let n = stream
                .read(&mut b)
                .await
                .map_err(|e| FcError::io("vsock reply", e))?;
            if n == 0 {
                return Err(FcError::Api("vsock handshake closed early".into()));
            }
            if b[0] == b'\n' {
                break;
            }
            line.push(b[0]);
            if line.len() > MAX_HANDSHAKE_LINE {
                return Err(FcError::Api("vsock handshake oversized".into()));
            }
        }
        let text = String::from_utf8_lossy(&line).into_owned();
        let trimmed = text.trim_end_matches('\r');
        if !trimmed.starts_with("OK ") {
            return Err(FcError::Api(format!(
                "vsock handshake refused: {trimmed:?}"
            )));
        }
        Ok((stream, Vec::new()))
    };
    tokio::time::timeout(timeout, fut)
        .await
        .map_err(|_| FcError::Timeout("vsock handshake".into()))?
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fake_vsock(path: &Path, reply: &'static [u8]) -> tokio::task::JoinHandle<usize> {
        let _ = std::fs::remove_file(path);
        let listener = tokio::net::UnixListener::bind(path).unwrap();
        tokio::spawn(async move {
            let mut conns = 0usize;
            while let Ok((mut stream, _)) = listener.accept().await {
                conns += 1;
                use tokio::io::AsyncReadExt;
                let mut buf = [0u8; 32];
                let _ = stream.read(&mut buf).await; // read CONNECT line
                use tokio::io::AsyncWriteExt;
                let _ = stream.write_all(reply).await;
                // Echo a bit of guest traffic to prove the stream is live.
                let _ = stream.write_all(b"ECHO").await;
                // keep open until test ends
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
            conns
        })
    }

    #[tokio::test]
    async fn ok_handshake_proceeds() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("vsock.uds");
        let h = fake_vsock(&p, b"OK 1234567890abcdef\n").await;
        let (mut stream, leftover) = dial_guest_vsock_buffered(&p, 52, Duration::from_secs(2))
            .await
            .unwrap();
        assert!(leftover.is_empty());
        let mut buf = [0u8; 4];
        let n = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&buf[..n], b"ECHO");
        h.abort();
    }

    #[tokio::test]
    async fn refused_handshake_errors() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("vsock.uds");
        let h = fake_vsock(&p, b"ERROR 12\n").await;
        let err = dial_guest_vsock_buffered(&p, 52, Duration::from_secs(2))
            .await
            .unwrap_err();
        assert!(matches!(err, FcError::Api(_)), "{err}");
        h.abort();
    }

    #[tokio::test]
    async fn hung_handshake_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("vsock.uds");
        let _ = std::fs::remove_file(&p);
        let listener = tokio::net::UnixListener::bind(&p).unwrap();
        let h = tokio::spawn(async move {
            while let Ok((_s, _)) = listener.accept().await {
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        });
        let err = dial_guest_vsock_buffered(&p, 52, Duration::from_millis(120))
            .await
            .unwrap_err();
        assert!(matches!(err, FcError::Timeout(_)), "{err}");
        h.abort();
    }
}
