//! Minimal HTTP/1.1 client for the Firecracker API over its Unix socket.
//!
//! Firecracker's API surface we use is exactly: `PUT /machine-config`,
//! `PUT /boot-source`, `PUT /drives/{id}`, `PUT /vsock`, `PUT /actions`,
//! and `PUT /logger`. A full HTTP stack is deliberately not pulled in: the
//! client is a length-bounded, single-request-per-connection writer/reader.
//! Any response anomaly (bad status line, over-cap body, truncated read) is
//! an `FcError::Api` — never a partial-success continuation.

use std::path::Path;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use crate::FcError;

/// Upper bound on a response body we are willing to read (Firecracker
/// bodies for our calls are tiny; 404s carry short JSON errors).
const MAX_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_HEADER_BYTES: usize = 16 * 1024;

pub struct FcApiClient {
    socket: std::path::PathBuf,
    request_timeout: Duration,
}

impl FcApiClient {
    #[must_use]
    pub fn new(socket: impl AsRef<Path>, request_timeout: Duration) -> Self {
        Self {
            socket: socket.as_ref().to_path_buf(),
            request_timeout,
        }
    }

    /// `PUT path` with a JSON body; expects 2xx (Firecracker returns
    /// 204 No Content for successful PUTs).
    pub async fn put(&self, path: &str, body: &serde_json::Value) -> Result<(), FcError> {
        let (status, _) = self.request("PUT", path, Some(body)).await?;
        if (200..300).contains(&status) {
            Ok(())
        } else {
            Err(FcError::Api(format!("PUT {path} -> HTTP {status}")))
        }
    }

    async fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<(u16, String), FcError> {
        let owned_body = match body {
            Some(v) => serde_json::to_vec(v).map_err(|e| FcError::Api(e.to_string()))?,
            None => Vec::new(),
        };
        let owned_body = &owned_body;
        tokio::time::timeout(
            self.request_timeout,
            self.request_inner(method, path, owned_body),
        )
        .await
        .map_err(|_| FcError::Timeout(format!("{method} {path}")))?
    }

    async fn request_inner(
        &self,
        method: &str,
        path: &str,
        body: &[u8],
    ) -> Result<(u16, String), FcError> {
        // Path hardening: only our fixed API paths, never caller-controlled
        // strings with traversal.
        if !path.starts_with('/')
            || path.contains("..")
            || path.contains('\n')
            || path.contains(' ')
        {
            return Err(FcError::Api(format!("invalid api path {path:?}")));
        }
        let mut stream = UnixStream::connect(&self.socket)
            .await
            .map_err(|e| FcError::io("fc api connect", e))?;
        let mut req = Vec::with_capacity(body.len() + 128);
        req.extend_from_slice(format!("{method} {path} HTTP/1.1\r\n").as_bytes());
        req.extend_from_slice(b"Host: localhost\r\n");
        req.extend_from_slice(format!("Content-Length: {}\r\n", body.len()).as_bytes());
        req.extend_from_slice(b"Content-Type: application/json\r\nConnection: close\r\n\r\n");
        req.extend_from_slice(body);
        stream
            .write_all(&req)
            .await
            .map_err(|e| FcError::io("fc api write", e))?;
        stream
            .flush()
            .await
            .map_err(|e| FcError::io("fc api flush", e))?;

        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        loop {
            let n = stream
                .read(&mut tmp)
                .await
                .map_err(|e| FcError::io("fc api read", e))?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
            if buf.len() > MAX_RESPONSE_BYTES {
                return Err(FcError::Api("response exceeds cap".into()));
            }
        }
        let split = buf
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .ok_or_else(|| FcError::Api("malformed response: no header terminator".into()))?;
        if split > MAX_HEADER_BYTES {
            return Err(FcError::Api("response headers exceed cap".into()));
        }
        let head = String::from_utf8_lossy(&buf[..split]).into_owned();
        let body_bytes = buf[split + 4..].to_vec();
        let first = head
            .lines()
            .next()
            .ok_or_else(|| FcError::Api("empty response".into()))?;
        let mut parts = first.splitn(3, ' ');
        let _http = parts.next().unwrap_or_default();
        let status: u16 = parts
            .next()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| FcError::Api(format!("bad status line {first:?}")))?;
        // Validate content-length against actual body when present.
        let declared: Option<usize> = head.lines().find_map(|l| {
            let l = l.to_ascii_lowercase();
            l.strip_prefix("content-length:")
                .and_then(|v| v.trim().parse().ok())
        });
        if let Some(declared) = declared
            && declared != body_bytes.len()
        {
            return Err(FcError::Api(format!(
                "content-length {declared} != body {}",
                body_bytes.len()
            )));
        }
        Ok((status, String::from_utf8_lossy(&body_bytes).into_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fake_fc(
        dir: &Path,
        responder: impl Fn(String) -> (u16, Vec<u8>) + Send + 'static,
    ) -> (FcApiClient, tokio::task::JoinHandle<()>) {
        let sock = dir.join("api.sock");
        let _ = std::fs::remove_file(&sock);
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let h = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = Vec::new();
                let mut t = [0u8; 4096];
                loop {
                    use tokio::io::AsyncReadExt;
                    let n = stream.read(&mut t).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&t[..n]);
                    if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let req = String::from_utf8_lossy(&buf).into_owned();
                let (code, body) = responder(req.clone());
                let resp = format!(
                    "HTTP/1.1 {code} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                use tokio::io::AsyncWriteExt;
                let _ = stream.write_all(resp.as_bytes()).await;
                let _ = stream.write_all(&body).await;
            }
        });
        (FcApiClient::new(&sock, Duration::from_secs(2)), h)
    }

    #[tokio::test]
    async fn put_ok_and_error_status() {
        let dir = tempfile::tempdir().unwrap();
        let (client, h) = fake_fc(dir.path(), |req| {
            if req.starts_with("PUT /machine-config") {
                (204, vec![])
            } else {
                (400, br#"{"fault_message":"bad"}"#.to_vec())
            }
        })
        .await;
        client
            .put("/machine-config", &serde_json::json!({"vcpu_count":2}))
            .await
            .unwrap();
        let err = client
            .put("/boot-source", &serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, FcError::Api(_)));
        h.abort();
    }

    #[tokio::test]
    async fn rejects_invalid_paths_and_content_length_lies() {
        let dir = tempfile::tempdir().unwrap();
        let (client, _h) = fake_fc(dir.path(), |_| (204, vec![])).await;
        assert!(matches!(
            client.put("/../etc", &serde_json::json!({})).await,
            Err(FcError::Api(_))
        ));
        assert!(matches!(
            client.put("/a b", &serde_json::json!({})).await,
            Err(FcError::Api(_))
        ));
        // content-length mismatch responder:
        let sock = dir.path().join("bad.sock");
        let _ = std::fs::remove_file(&sock);
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let h = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut t = [0u8; 4096];
                let _ = stream.read(&mut t).await;
                let _ = stream
                    .write_all(b"HTTP/1.1 204 X\r\nContent-Length: 9\r\n\r\nabc")
                    .await;
            }
        });
        let client = FcApiClient::new(&sock, Duration::from_secs(2));
        assert!(matches!(
            client.put("/x", &serde_json::json!({})).await,
            Err(FcError::Api(_))
        ));
        h.abort();
    }

    #[tokio::test]
    async fn timeout_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("slow.sock");
        let _ = std::fs::remove_file(&sock);
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let h = tokio::spawn(async move {
            while let Ok((_stream, _)) = listener.accept().await {
                // never respond
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        });
        let client = FcApiClient::new(&sock, Duration::from_millis(150));
        let start = std::time::Instant::now();
        assert!(matches!(
            client.put("/x", &serde_json::json!({})).await,
            Err(FcError::Timeout(_))
        ));
        assert!(start.elapsed() < Duration::from_secs(2));
        h.abort();
    }
}
