//! HTTP/1.1 over the runtime's unix socket: one connection per call, a
//! deadline on the whole exchange and a cap on the response body.

use std::path::PathBuf;
use std::time::Duration;

use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Bytes;
use hyper::header::{CONTENT_TYPE, HOST};
use hyper::{Method, Request};
use hyper_util::rt::TokioIo;
use tokio::net::UnixStream;
use tokio::task::JoinHandle;

use crate::error::bounded;
use crate::runtime::RuntimeError;

/// Largest response body read; Podman's `/system/df` lists every image.
const BODY_MAX_BYTES: usize = 16 * 1024 * 1024;

/// A response: status and the whole body.
#[derive(Debug)]
pub(crate) struct Response {
    pub status: u16,
    pub body: Bytes,
}

/// Calls to one runtime socket.
#[derive(Debug, Clone)]
pub(crate) struct Transport {
    socket: PathBuf,
}

/// Aborts the connection task when the call ends, finished or not.
struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl Transport {
    pub(crate) fn new(socket: PathBuf) -> Self {
        Self { socket }
    }

    /// Send one request with an optional JSON body and read the answer
    /// within `timeout`.
    pub(crate) async fn call(
        &self,
        method: Method,
        path: &str,
        body: Option<Vec<u8>>,
        timeout: Duration,
    ) -> Result<Response, RuntimeError> {
        match tokio::time::timeout(timeout, self.exchange(method, path, body)).await {
            Ok(result) => result,
            Err(_) => Err(RuntimeError::Timeout),
        }
    }

    async fn exchange(
        &self,
        method: Method,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> Result<Response, RuntimeError> {
        let stream = UnixStream::connect(&self.socket)
            .await
            .map_err(|error| RuntimeError::Unreachable(bounded(&error.to_string())))?;
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .map_err(unreachable)?;
        let _driver = AbortOnDrop(tokio::spawn(async move {
            let _ = connection.await;
        }));
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header(HOST, "localhost")
            .header(CONTENT_TYPE, "application/json")
            .body(Full::new(Bytes::from(body.unwrap_or_default())))
            .map_err(|error| RuntimeError::Malformed(bounded(&error.to_string())))?;
        let response = sender.send_request(request).await.map_err(unreachable)?;
        let status = response.status().as_u16();
        let body = Limited::new(response.into_body(), BODY_MAX_BYTES)
            .collect()
            .await
            .map_err(|_| RuntimeError::Malformed("response body unreadable or too large".into()))?
            .to_bytes();
        Ok(Response { status, body })
    }
}

fn unreachable(error: hyper::Error) -> RuntimeError {
    RuntimeError::Unreachable(bounded(&error.to_string()))
}

/// Percent-encode `value` for a query string: everything but RFC 3986
/// unreserved characters.
pub(crate) fn encode_query(value: &str) -> String {
    let mut out = String::with_capacity(value.len() * 3);
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}
