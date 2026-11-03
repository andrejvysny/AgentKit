//! JSON-RPC 2.0 over stdio with `Content-Length` framing.
//!
//! The [`Connection`] owns one bidirectional pipe pair to a language
//! server. A single reader task routes incoming frames: responses wake the
//! pending request registered under the same id, notifications go to the
//! registered handler, and server-initiated requests (`window/…`,
//! `window/workDoneProgress/create`, …) are answered through the
//! incoming-request hook on a spawned task so a slow hook can never
//! deadlock the reader. Malformed frames are protocol errors that mark the
//! connection broken and fail every in-flight request.

use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicI64, Ordering},
    },
    time::Duration,
};

use agentkit_core::CancellationToken;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    sync::{Notify, oneshot},
};

/// Hard cap on the size of a header block (bytes before `\r\n\r\n`).
pub const MAX_HEADER_BLOCK_BYTES: usize = 64 * 1024;
/// Hard cap on a single message body.
pub const MAX_BODY_BYTES: usize = 128 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RpcErrorObject {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// Why a request did not complete.
#[derive(Clone, Debug, PartialEq)]
pub enum RpcFailure {
    /// The connection is gone (EOF, write failure or protocol error);
    /// carries the human-readable reason.
    Disconnected(String),
    /// The server answered with a JSON-RPC error object.
    Rpc {
        code: i64,
        message: String,
        data: Option<Value>,
    },
    /// The per-request deadline expired; `$/cancelRequest` was sent.
    Timeout,
    /// The caller's token fired; `$/cancelRequest` was sent.
    Cancelled,
}

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;
/// Hook for server-initiated requests; its value becomes the `result`
/// field of the reply (errors reply with the given JSON-RPC error).
pub type RequestHook =
    Arc<dyn Fn(String, Option<Value>) -> BoxFuture<Result<Value, RpcErrorObject>> + Send + Sync>;
/// Handler for server notifications (publishDiagnostics, $/progress…).
pub type NotificationHandler = Arc<dyn Fn(String, Option<Value>) + Send + Sync>;

/// Encode one framed message (`Content-Length` header + blank line + body).
pub fn encode_frame(body: &Value) -> Vec<u8> {
    let payload = serde_json::to_vec(body).expect("Value always serializes");
    let mut out = Vec::with_capacity(payload.len() + 32);
    out.extend_from_slice(format!("Content-Length: {}\r\n", payload.len()).as_bytes());
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(&payload);
    out
}

/// Read one frame. Returns `Ok(None)` on clean EOF at a frame boundary,
/// `Err(reason)` on any protocol violation (which invalidates the stream).
pub async fn read_frame<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
) -> Result<Option<Value>, String> {
    let mut content_length: Option<usize> = None;
    let mut header_bytes = 0usize;
    loop {
        let mut line = String::new();
        let read = reader
            .read_line(&mut line)
            .await
            .map_err(|e| format!("header read failed: {e}"))?;
        if read == 0 {
            return if header_bytes == 0 {
                Ok(None)
            } else {
                Err("connection closed inside the header block".into())
            };
        }
        header_bytes += line.len();
        if header_bytes > MAX_HEADER_BLOCK_BYTES {
            return Err(format!(
                "header block exceeds {MAX_HEADER_BLOCK_BYTES} bytes"
            ));
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| format!("malformed header line: {line:?}"))?;
        if name.trim().eq_ignore_ascii_case("content-length") {
            let parsed: usize = value
                .trim()
                .parse()
                .map_err(|_| format!("invalid Content-Length: {:?}", value.trim()))?;
            if parsed > MAX_BODY_BYTES {
                return Err(format!("message body of {parsed} bytes exceeds the cap"));
            }
            content_length = Some(parsed);
        }
        // All other headers (Content-Type) are accepted and ignored.
    }
    let length = content_length.ok_or_else(|| "frame without Content-Length".to_string())?;
    let mut body = vec![0u8; length];
    reader
        .read_exact(&mut body)
        .await
        .map_err(|e| format!("body read failed after {length} bytes: {e}"))?;
    let value: Value =
        serde_json::from_slice(&body).map_err(|e| format!("body is not valid JSON: {e}"))?;
    Ok(Some(value))
}

struct Pending {
    tx: oneshot::Sender<Result<Value, RpcFailure>>,
}

/// A live JSON-RPC connection plus its reader task.
pub struct Connection {
    writer: tokio::sync::Mutex<Box<dyn AsyncWrite + Unpin + Send>>,
    next_id: AtomicI64,
    pending: Mutex<HashMap<Value, Pending>>,
    broken: Mutex<Option<String>>,
    broken_notify: Notify,
    on_notification: NotificationHandler,
    on_request: RequestHook,
}

impl Connection {
    /// Wrap a pipe pair and start routing frames. The reader task runs
    /// until the peer closes or a frame violates the protocol.
    pub fn attach(
        reader: impl AsyncRead + Unpin + Send + 'static,
        writer: impl AsyncWrite + Unpin + Send + 'static,
        on_notification: NotificationHandler,
        on_request: RequestHook,
    ) -> Arc<Self> {
        let conn = Arc::new(Self {
            writer: tokio::sync::Mutex::new(Box::new(writer)),
            next_id: AtomicI64::new(1),
            pending: Mutex::new(HashMap::new()),
            broken: Mutex::new(None),
            broken_notify: Notify::new(),
            on_notification,
            on_request,
        });
        let reader_conn = Arc::clone(&conn);
        tokio::spawn(async move {
            let mut buf = BufReader::new(reader);
            loop {
                match read_frame(&mut buf).await {
                    Ok(None) => {
                        reader_conn.mark_broken("server closed the connection (EOF)".into());
                        break;
                    }
                    Ok(Some(frame)) => Arc::clone(&reader_conn).dispatch(frame),
                    Err(reason) => {
                        reader_conn.mark_broken(format!("protocol error: {reason}"));
                        break;
                    }
                }
            }
        });
        conn
    }

    pub fn is_broken(&self) -> Option<String> {
        self.broken.lock().expect("broken lock").clone()
    }

    fn mark_broken(&self, reason: String) {
        {
            let mut broken = self.broken.lock().expect("broken lock");
            if broken.is_none() {
                *broken = Some(reason.clone());
            }
        }
        let drained: Vec<Pending> = self
            .pending
            .lock()
            .expect("pending lock")
            .drain()
            .map(|(_, entry)| entry)
            .collect();
        for entry in drained {
            let _ = entry.tx.send(Err(RpcFailure::Disconnected(reason.clone())));
        }
        self.broken_notify.notify_waiters();
    }

    async fn write_frame(self: &Arc<Self>, frame: &Value) -> Result<(), RpcFailure> {
        let bytes = encode_frame(frame);
        let mut writer = self.writer.lock().await;
        let written = writer.write_all(&bytes).await;
        let outcome = match written {
            Ok(()) => writer.flush().await,
            Err(e) => Err(e),
        };
        match outcome {
            Ok(()) => Ok(()),
            Err(e) => {
                drop(writer);
                self.mark_broken(format!("write failed: {e}"));
                Err(RpcFailure::Disconnected(format!("write failed: {e}")))
            }
        }
    }

    /// `write_frame` under a deadline (and optional cancellation). A live
    /// server that stops draining stdin fills the OS pipe; an unbounded
    /// `write_all` then blocks forever HOLDING the writer mutex — no
    /// request timeout or caller cancellation can fire, and every later
    /// frame of the generation (`$/cancelRequest`, `shutdown`, `exit`)
    /// queues behind it, hanging `ServerProcess::shutdown` before it ever
    /// reaches `start_kill`. A stalled write means the byte stream is no
    /// longer trustworthy: mark the connection broken so the pool respawns.
    async fn write_frame_bounded(
        self: &Arc<Self>,
        frame: &Value,
        deadline: Duration,
        cancellation: Option<&CancellationToken>,
    ) -> Result<(), RpcFailure> {
        let outcome = match cancellation {
            Some(token) => {
                tokio::select! {
                    result = self.write_frame(frame) => result,
                    _ = tokio::time::sleep(deadline) => Err(RpcFailure::Timeout),
                    _ = token.cancelled() => Err(RpcFailure::Cancelled),
                }
            }
            None => {
                tokio::select! {
                    result = self.write_frame(frame) => result,
                    _ = tokio::time::sleep(deadline) => Err(RpcFailure::Timeout),
                }
            }
        };
        if outcome.is_err() {
            self.mark_broken("write stalled or abandoned mid-frame".into());
        }
        outcome
    }

    fn dispatch(self: Arc<Self>, frame: Value) {
        let method = frame.get("method").and_then(Value::as_str);
        let id = frame.get("id").cloned();
        match (method, id) {
            (Some(method), Some(id)) => {
                let method = method.to_owned();
                let params = frame.get("params").cloned();
                let hook = Arc::clone(&self.on_request);
                // Spawned: a slow or hanging incoming-request handler must
                // never block the reader task.
                tokio::spawn(async move {
                    let reply = match hook(method, params).await {
                        Ok(result) => json!({"jsonrpc":"2.0","id":id,"result":result}),
                        Err(error) => json!({"jsonrpc":"2.0","id":id,
                            "error":{"code":error.code,"message":error.message,
                                      "data":error.data}}),
                    };
                    let _ = self.write_frame(&reply).await;
                });
            }
            (Some(method), None) => {
                let method = method.to_owned();
                let params = frame.get("params").cloned();
                let handler = Arc::clone(&self.on_notification);
                tokio::spawn(async move { handler(method, params) });
            }
            (None, Some(id)) => {
                let entry = self.pending.lock().expect("pending lock").remove(&id);
                let Some(entry) = entry else { return }; // late/unknown reply
                let outcome = if let Some(error) = frame.get("error") {
                    Err(RpcFailure::Rpc {
                        code: error.get("code").and_then(Value::as_i64).unwrap_or(-1),
                        message: error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown error")
                            .to_owned(),
                        data: error.get("data").cloned(),
                    })
                } else {
                    Ok(frame.get("result").cloned().unwrap_or(Value::Null))
                };
                let _ = entry.tx.send(outcome);
            }
            (None, None) => {
                self.mark_broken("frame is neither request, response nor notification".into());
            }
        }
    }

    /// Send a request and await its reply. Honours the caller's
    /// cancellation token and the deadline; both send `$/cancelRequest`.
    pub async fn request(
        self: &Arc<Self>,
        method: &str,
        params: Value,
        timeout: Duration,
        cancellation: CancellationToken,
    ) -> Result<Value, RpcFailure> {
        if let Some(reason) = self.is_broken() {
            return Err(RpcFailure::Disconnected(reason));
        }
        let id = Value::from(self.next_id.fetch_add(1, Ordering::SeqCst));
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .expect("pending lock")
            .insert(id.clone(), Pending { tx });
        if let Some(reason) = self.is_broken() {
            self.pending.lock().expect("pending lock").remove(&id);
            return Err(RpcFailure::Disconnected(reason));
        }
        let frame = json!({"jsonrpc":"2.0","id":id,"method":method,
                           "params":if params.is_null() {json!({})} else {params}});
        if let Err(error) = self
            .write_frame_bounded(&frame, timeout, Some(&cancellation))
            .await
        {
            self.pending.lock().expect("pending lock").remove(&id);
            return Err(error);
        }

        let broken_wait = self.broken_notify.notified();
        tokio::pin!(broken_wait);
        let outcome = tokio::select! {
            reply = rx => match reply {
                Ok(outcome) => outcome,
                Err(_) => Err(RpcFailure::Disconnected("request abandoned".into())),
            },
            _ = cancellation.cancelled() => Err(RpcFailure::Cancelled),
            _ = tokio::time::sleep(timeout) => Err(RpcFailure::Timeout),
            _ = &mut broken_wait => Err(self.is_broken()
                .map(RpcFailure::Disconnected)
                .unwrap_or_else(|| RpcFailure::Disconnected("connection broken".into()))),
        };
        if matches!(outcome, Err(RpcFailure::Cancelled | RpcFailure::Timeout)) {
            self.pending.lock().expect("pending lock").remove(&id);
            let _ = self.notify("$/cancelRequest", json!({"id": id})).await;
        }
        outcome
    }

    /// Fire-and-forget notification. Bounded write: shutdown paths send
    /// `exit` through here and must reach `start_kill` even against a
    /// server that stopped draining stdin.
    pub async fn notify(self: &Arc<Self>, method: &str, params: Value) -> Result<(), RpcFailure> {
        if let Some(reason) = self.is_broken() {
            return Err(RpcFailure::Disconnected(reason));
        }
        self.write_frame_bounded(
            &json!({"jsonrpc":"2.0","method":method,
                    "params":if params.is_null() {json!({})} else {params}}),
            Duration::from_secs(5),
            None,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{ReadHalf, WriteHalf, duplex, split};

    fn null_hook() -> RequestHook {
        Arc::new(|_m, _p| Box::pin(async { Ok(Value::Null) }))
    }
    fn noop_notify() -> NotificationHandler {
        Arc::new(|_m, _p| {})
    }
    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    /// One bidirectional pipe: the connection owns one end, the test the
    /// other (split so reader and writer tasks are independent).
    struct Peer {
        conn: Arc<Connection>,
        read: BufReader<ReadHalf<DuplexStream>>,
        write: WriteHalf<DuplexStream>,
    }
    use tokio::io::DuplexStream;

    fn peer() -> Peer {
        peer_with(noop_notify(), null_hook())
    }
    fn peer_with(notification: NotificationHandler, request: RequestHook) -> Peer {
        let (test_side, conn_side) = duplex(64 * 1024);
        let (cr, cw) = split(conn_side);
        let (tr, tw) = split(test_side);
        Peer {
            conn: Connection::attach(cr, cw, notification, request),
            read: BufReader::new(tr),
            write: tw,
        }
    }

    #[test]
    fn encode_shapes_the_header() {
        let msg = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"a":1}});
        let bytes = encode_frame(&msg);
        assert!(bytes.starts_with(b"Content-Length: "));
        let sep = b"\r\n\r\n";
        assert!(bytes.windows(sep.len()).any(|window| window == &sep[..]),);
    }

    #[tokio::test]
    async fn encode_round_trips_through_decoder() {
        let msg = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"a":1}});
        let bytes = encode_frame(&msg);
        let mut reader = BufReader::new(&bytes[..]);
        let decoded = read_frame(&mut reader).await.unwrap().unwrap();
        assert_eq!(decoded, msg);
        assert!(read_frame(&mut reader).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn frame_survives_byte_split_reads() {
        let msg = json!({"jsonrpc":"2.0","method":"x","params":{"k":"日本"}});
        let bytes = encode_frame(&msg);
        let (test_side, conn_side) = duplex(64);
        let (cr, cw) = split(conn_side);
        let writer = tokio::spawn(async move {
            let mut client = test_side;
            for chunk in bytes.chunks(3) {
                client.write_all(chunk).await.unwrap();
                tokio::task::yield_now().await;
            }
            client.shutdown().await.unwrap();
            cw
        });
        let mut reader = BufReader::new(cr);
        let decoded = read_frame(&mut reader).await.unwrap().unwrap();
        assert_eq!(decoded, msg);
        let _ = writer.await.unwrap();
    }

    #[tokio::test]
    async fn headers_are_case_insensitive() {
        let body = br#"{"jsonrpc":"2.0","method":"a"}"#.to_vec();
        let mut all = format!(
            "content-LENGTH: {}\r\nConTent-TyPe: application/vscode-jsonrpc; charset=utf-8\r\n\r\n",
            body.len()
        )
        .into_bytes();
        all.extend_from_slice(&body);
        let mut reader = BufReader::new(&all[..]);
        assert_eq!(
            read_frame(&mut reader).await.unwrap().unwrap(),
            json!({"jsonrpc":"2.0","method":"a"})
        );
    }

    #[tokio::test]
    async fn oversized_header_block_is_rejected() {
        let mut raw = b"Content-Length: 5\r\nX-Junk: ".to_vec();
        raw.extend_from_slice(vec![b'a'; MAX_HEADER_BLOCK_BYTES + 10].as_slice());
        raw.extend_from_slice(b"\r\n\r\nhello");
        let mut reader = BufReader::new(&raw[..]);
        let err = read_frame(&mut reader).await.unwrap_err();
        assert!(err.contains("header block"), "{err}");
    }

    #[tokio::test]
    async fn garbage_body_is_protocol_error() {
        let raw = b"Content-Length: 4\r\n\r\nnope";
        let mut reader = BufReader::new(&raw[..]);
        assert!(read_frame(&mut reader).await.unwrap_err().contains("JSON"));
    }

    #[tokio::test]
    async fn missing_content_length_rejected() {
        let raw = b"Content-Type: x\r\n\r\n{}";
        let mut reader = BufReader::new(&raw[..]);
        assert!(
            read_frame(&mut reader)
                .await
                .unwrap_err()
                .contains("Content-Length")
        );
    }

    #[tokio::test]
    async fn concurrent_requests_interleave_by_id() {
        let mut p = peer();
        let echo = tokio::spawn(async move {
            let (mut tr, tw) = (p.read, &mut p.write);
            let mut frames = Vec::new();
            for _ in 0..3 {
                frames.push(read_frame(&mut tr).await.unwrap().unwrap());
            }
            for frame in frames.into_iter().rev() {
                let reply = json!({"jsonrpc":"2.0","id":frame["id"].clone(),
                                   "result":{"method":frame["method"].clone()}});
                tw.write_all(&encode_frame(&reply)).await.unwrap();
            }
            tr
        });
        let (a, b, c) = tokio::join!(
            p.conn
                .request("m/a", json!({}), secs(5), CancellationToken::new()),
            p.conn
                .request("m/b", json!({}), secs(5), CancellationToken::new()),
            p.conn
                .request("m/c", json!({}), secs(5), CancellationToken::new()),
        );
        assert_eq!(a.unwrap()["method"], "m/a");
        assert_eq!(b.unwrap()["method"], "m/b");
        assert_eq!(c.unwrap()["method"], "m/c");
        drop(echo);
    }

    #[tokio::test]
    async fn timeout_sends_cancel_and_fails() {
        let mut p = peer();
        let token = CancellationToken::new();
        let call = {
            let conn = Arc::clone(&p.conn);
            tokio::spawn(async move {
                conn.request("slow", json!(null), Duration::from_millis(50), token)
                    .await
            })
        };
        let frame = read_frame(&mut p.read).await.unwrap().unwrap();
        assert_eq!(frame["method"], "slow");
        let cancel = read_frame(&mut p.read).await.unwrap().unwrap();
        assert_eq!(cancel["method"], "$/cancelRequest");
        assert_eq!(cancel["params"]["id"], frame["id"]);
        assert_eq!(call.await.unwrap(), Err(RpcFailure::Timeout));
        // Connection stays usable for notifications after a timeout.
        p.conn.notify("ping", json!(null)).await.unwrap();
        let ping = read_frame(&mut p.read).await.unwrap().unwrap();
        assert_eq!(ping["method"], "ping");
    }

    #[tokio::test]
    async fn caller_cancellation_resolves_cancelled() {
        let mut p = peer();
        let token = CancellationToken::new();
        let t = token.clone();
        let call = {
            let conn = Arc::clone(&p.conn);
            tokio::spawn(async move { conn.request("slow", json!(null), secs(30), t).await })
        };
        let frame = read_frame(&mut p.read).await.unwrap().unwrap();
        token.cancel();
        let cancel = read_frame(&mut p.read).await.unwrap().unwrap();
        assert_eq!(cancel["method"], "$/cancelRequest");
        assert_eq!(cancel["params"]["id"], frame["id"]);
        assert_eq!(call.await.unwrap(), Err(RpcFailure::Cancelled));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stalled_write_times_out_instead_of_hanging() {
        // A wedged server stops draining stdin: a big frame fills the
        // duplex buffer and `write_all` parks HOLDING the writer mutex.
        // Unbounded, that hang outlives the request timeout, the caller's
        // cancellation, and every shutdown frame queued behind it — the
        // generation could never be killed. Bounded writes fail the
        // request and mark the stream untrustworthy.
        let p = peer();
        let conn = Arc::clone(&p.conn);
        let blob = "x".repeat(256 * 1024);
        let result = tokio::time::timeout(
            secs(5),
            conn.request(
                "big/ping",
                json!({"blob": blob}),
                secs(1),
                CancellationToken::new(),
            ),
        )
        .await
        .expect("a stalled write must not outlive its deadline")
        .expect_err("stalled write is a timeout");
        assert_eq!(result, RpcFailure::Timeout);
        assert!(
            p.conn.is_broken().is_some(),
            "a connection whose writer stalled must be marked broken for the pool to respawn it"
        );

        // The same stall must be releasable by caller cancellation too,
        // not just by the deadline.
        let p2 = peer();
        let token = CancellationToken::new();
        let t = token.clone();
        let conn2 = Arc::clone(&p2.conn);
        let call = tokio::spawn(async move {
            conn2
                .request(
                    "big/ping",
                    json!({"blob": "y".repeat(256 * 1024)}),
                    secs(30),
                    t,
                )
                .await
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        token.cancel();
        assert_eq!(
            tokio::time::timeout(secs(5), call)
                .await
                .expect("cancellation must release the stalled write")
                .unwrap(),
            Err(RpcFailure::Cancelled)
        );
    }

    #[tokio::test]
    async fn eof_fails_pending_with_disconnected() {
        let (test_side, conn_side) = duplex(64);
        let (cr, cw) = split(conn_side);
        drop(test_side); // peer closes both directions
        let conn = Connection::attach(cr, cw, noop_notify(), null_hook());
        tokio::time::sleep(Duration::from_millis(20)).await;
        let err = conn
            .request("m", json!(null), secs(5), CancellationToken::new())
            .await
            .unwrap_err();
        assert!(matches!(err, RpcFailure::Disconnected(_)));
        assert!(conn.is_broken().unwrap().contains("EOF"));
    }

    #[tokio::test]
    async fn incoming_request_answered_with_null_and_hook() {
        let hook: RequestHook = Arc::new(|method, _p| {
            Box::pin(async move {
                if method == "window/workDoneProgress/create" {
                    Ok(Value::Null)
                } else {
                    Err(RpcErrorObject {
                        code: -32601,
                        message: "no such handler".into(),
                        data: None,
                    })
                }
            })
        });
        let mut p = peer_with(noop_notify(), hook);
        p.write
            .write_all(&encode_frame(&json!({"jsonrpc":"2.0","id":7,
                "method":"window/workDoneProgress/create","params":{}})))
            .await
            .unwrap();
        let reply = read_frame(&mut p.read).await.unwrap().unwrap();
        assert_eq!(reply["id"], 7);
        assert_eq!(reply["result"], Value::Null);

        p.write
            .write_all(&encode_frame(&json!({"jsonrpc":"2.0","id":"s1",
                "method":"client/hooks"})))
            .await
            .unwrap();
        let reply = read_frame(&mut p.read).await.unwrap().unwrap();
        assert_eq!(reply["id"], "s1");
        assert_eq!(reply["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn slow_incoming_request_handler_does_not_block_reader() {
        let hook: RequestHook = Arc::new(|_m, _p| {
            Box::pin(async {
                tokio::time::sleep(Duration::from_millis(150)).await;
                Ok(Value::Null)
            })
        });
        let mut p = peer_with(noop_notify(), hook);
        // Server sends a slow-handler request; while that handler runs,
        // a response to a client request must still be routed.
        p.write
            .write_all(&encode_frame(&json!({"jsonrpc":"2.0","id":99,
                "method":"window/workDoneProgress/create"})))
            .await
            .unwrap();
        let call = {
            let conn = Arc::clone(&p.conn);
            async move {
                conn.request("m", json!(null), secs(5), CancellationToken::new())
                    .await
            }
        };
        let _hanging = read_frame(&mut p.read).await.unwrap().unwrap();
        p.write
            .write_all(&encode_frame(&json!({"jsonrpc":"2.0","id":1,
                "result":{"fast":true}})))
            .await
            .unwrap();
        let answer = call.await.unwrap();
        assert_eq!(answer["fast"], true);
    }

    #[tokio::test]
    async fn unknown_notifications_are_ignored_and_known_routed() {
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = Arc::clone(&seen);
        let handler: NotificationHandler = Arc::new(move |m, _p| sink.lock().unwrap().push(m));
        let mut p = peer_with(handler, null_hook());
        p.write
            .write_all(&encode_frame(&json!({"jsonrpc":"2.0",
                "method":"$/progress","params":{}})))
            .await
            .unwrap();
        p.write
            .write_all(&encode_frame(
                &json!({"jsonrpc":"2.0","method":"totally/unknown"}),
            ))
            .await
            .unwrap();
        for _ in 0..20 {
            if seen.lock().unwrap().len() == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let got = seen.lock().unwrap().clone();
        assert_eq!(got, vec!["$/progress", "totally/unknown"]);
        // Nothing was written back for notifications.
        p.conn.notify("client/next", json!(null)).await.unwrap();
        let next = read_frame(&mut p.read).await.unwrap().unwrap();
        assert_eq!(next["method"], "client/next");
    }

    #[tokio::test]
    async fn garbage_frame_marks_connection_broken() {
        let mut p = peer();
        p.write
            .write_all(b"Content-Length: 4\r\n\r\nnope")
            .await
            .unwrap();
        for _ in 0..20 {
            if p.conn.is_broken().is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(p.conn.is_broken().unwrap().contains("protocol error"));
        let err = p
            .conn
            .request("m", json!(null), secs(5), CancellationToken::new())
            .await
            .unwrap_err();
        assert!(matches!(err, RpcFailure::Disconnected(_)));
    }
}
