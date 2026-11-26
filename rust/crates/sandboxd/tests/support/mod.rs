//! A test host: starts a supervisor in this process over a socket pair and
//! speaks the guest protocol to it, one request at a time.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use agentkit_sandbox_proto::guest::{
    ExecRequest, FileData, GuestError, OperationState, OutputData, OutputLimit, PathRoot,
    ReadRequest, Reply, ReplyEnvelope, Request, WriteRequest,
};
use agentkit_sandbox_proto::{
    DataFrame, Envelope, Frame, FrameDecoder, PROTOCOL_VERSION, data_frames, encode_control,
    encode_data, parse_control,
};
use agentkit_sandboxd::{Config, Exit, HandshakeError, ProcessMode, serve};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::task::JoinHandle;

pub const GENERATION: u64 = 7;
pub const DIGEST: &str = "sha256:test-image";
pub const CHANNEL_TOKEN: &str = "test-channel-token";
pub const LOG_LIMIT: OutputLimit = OutputLimit {
    max_bytes: 64 << 20,
    max_lines: None,
};
/// How long a test waits for a receipt.
const RECEIPT_WAIT: Duration = Duration::from_secs(20);

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The guest's two roots and environment file in a scratch directory.
pub struct Guest {
    pub dir: tempfile::TempDir,
    pub workspace: PathBuf,
    pub home: PathBuf,
}

impl Guest {
    pub fn new(env: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        let home = dir.path().join("home");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(&home).unwrap();
        std::fs::write(dir.path().join("environment"), env).unwrap();
        Self {
            dir,
            workspace,
            home,
        }
    }

    pub fn config(&self, mode: ProcessMode) -> Config {
        Config {
            generation: GENERATION,
            image_digest: DIGEST.to_owned(),
            channel_token: CHANNEL_TOKEN.to_owned(),
            control_socket: self.dir.path().join("control.sock"),
            egress_socket: self.dir.path().join("egress.sock"),
            workspace_root: self.workspace.clone(),
            home_root: self.home.clone(),
            env_file: self.dir.path().join("environment"),
            shell: PathBuf::from("/bin/bash"),
            shim_addr: "127.0.0.1:0".parse().unwrap(),
            mode,
        }
    }
}

/// The host end of one channel.
pub struct Host {
    stream: UnixStream,
    decoder: FrameDecoder,
    next_id: u64,
    pub serving: JoinHandle<Result<Exit, HandshakeError>>,
}

/// A command's receipt and all its output.
#[derive(Debug)]
pub struct Finished {
    pub state: OperationState,
    pub output: Vec<u8>,
    pub truncated: bool,
}

/// Start a supervisor on `guest` and open a channel to it, not greeted.
pub fn connect(guest: &Guest, mode: ProcessMode) -> Host {
    let (host, guest_end) = UnixStream::pair().unwrap();
    let serving = tokio::spawn(serve(guest.config(mode), guest_end));
    Host {
        stream: host,
        decoder: FrameDecoder::new(),
        next_id: 1,
        serving,
    }
}

/// A greeted channel to a supervisor in Embedded mode.
pub async fn start(guest: &Guest) -> Host {
    start_in(guest, ProcessMode::Embedded).await
}

pub async fn start_in(guest: &Guest, mode: ProcessMode) -> Host {
    let mut host = connect(guest, mode);
    let reply = host
        .request(Request::Hello {
            protocol: PROTOCOL_VERSION,
            generation: GENERATION,
        })
        .await;
    assert_eq!(
        reply,
        Reply::Hello {
            protocol: PROTOCOL_VERSION,
            generation: GENERATION,
            image_digest: DIGEST.to_owned(),
            channel_token: CHANNEL_TOKEN.to_owned(),
        }
    );
    host
}

pub fn exec_request(op: &str, command: &str) -> ExecRequest {
    ExecRequest {
        op: op.to_owned(),
        args_sha256: sha256_hex(command.as_bytes()),
        command: command.to_owned(),
        cwd: String::new(),
        timeout_ms: 120_000,
        output_limit: LOG_LIMIT,
        tz: None,
    }
}

pub fn read_request(root: PathRoot, path: &str) -> ReadRequest {
    ReadRequest {
        root,
        path: path.to_owned(),
        offset: 0,
        limit_bytes: 10 << 20,
        max_file_bytes: 10 << 20,
        raw: false,
    }
}

impl Host {
    pub async fn send_frame(&mut self, bytes: &[u8]) {
        self.stream.write_all(bytes).await.unwrap();
    }

    /// Send `body` with a fresh id.
    pub async fn send(&mut self, body: Request) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let frame = encode_control(&Envelope::new(id, body)).unwrap();
        self.send_frame(&frame).await;
        id
    }

    pub async fn send_data(&mut self, id: u64, bytes: &[u8]) {
        for DataFrame { id, seq, bytes } in data_frames(id, bytes) {
            let frame = encode_data(id, seq, &bytes).unwrap();
            self.send_frame(&frame).await;
        }
    }

    /// The next frame, `None` when the guest closed the channel.
    pub async fn next_frame(&mut self) -> Option<Frame> {
        let mut buf = vec![0; 64 * 1024];
        loop {
            if let Some(frame) = self.decoder.next_frame().unwrap() {
                return Some(frame);
            }
            let read = tokio::time::timeout(RECEIPT_WAIT, self.stream.read(&mut buf))
                .await
                .expect("the guest answers")
                .unwrap_or(0);
            if read == 0 {
                return None;
            }
            self.decoder.push(&buf[..read]);
        }
    }

    /// The reply to request `id`, skipping anything else.
    pub async fn reply_to(&mut self, id: u64) -> Reply {
        loop {
            match self.next_frame().await.expect("the channel is open") {
                Frame::Control(json) => {
                    let reply: ReplyEnvelope = parse_control(&json).unwrap();
                    if reply.id == id {
                        return reply.body;
                    }
                }
                Frame::Data(_) => {}
            }
        }
    }

    /// The data stream of request `id`, up to its terminator.
    pub async fn stream_of(&mut self, id: u64) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut seq = 0;
        loop {
            match self.next_frame().await.expect("the channel is open") {
                Frame::Data(data) if data.id == id => {
                    assert_eq!(data.seq, seq, "data frames arrive in order");
                    seq += 1;
                    if data.bytes.is_empty() {
                        return bytes;
                    }
                    bytes.extend_from_slice(&data.bytes);
                }
                _ => {}
            }
        }
    }

    pub async fn request(&mut self, body: Request) -> Reply {
        let id = self.send(body).await;
        self.reply_to(id).await
    }

    pub async fn exec(&mut self, req: ExecRequest) -> Reply {
        self.request(Request::Exec(req)).await
    }

    pub async fn output(
        &mut self,
        op: &str,
        after_seq: u64,
        max_bytes: u64,
    ) -> (OutputData, Vec<u8>) {
        let id = self
            .send(Request::Output {
                op: op.to_owned(),
                after_seq,
                max_bytes,
            })
            .await;
        match self.reply_to(id).await {
            Reply::OutputData(header) => {
                let bytes = self.stream_of(id).await;
                assert_eq!(bytes.len() as u64, header.len);
                (header, bytes)
            }
            other => panic!("expected output_data, got {other:?}"),
        }
    }

    /// Poll `op` until its receipt and every output byte are read; each
    /// reply must carry at most `per_call_max` bytes.
    pub async fn finish_bounded(&mut self, op: &str, per_call_max: u64) -> Finished {
        let deadline = Instant::now() + RECEIPT_WAIT;
        let (mut seq, mut output, mut truncated) = (0, Vec::new(), false);
        loop {
            let (seen, bytes) = self.output(op, seq, 1 << 20).await;
            assert!(bytes.len() as u64 <= per_call_max, "observe is bounded");
            assert!(seen.next_seq >= seq, "the sequence never goes back");
            seq = seen.next_seq;
            output.extend_from_slice(&bytes);
            truncated |= seen.truncated;
            if seen.eof {
                assert!(is_terminal(seen.state), "eof only with a receipt");
                return Finished {
                    state: seen.state,
                    output,
                    truncated,
                };
            }
            assert!(Instant::now() < deadline, "{op} has no receipt in time");
            if bytes.is_empty() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }

    pub async fn finish(&mut self, op: &str) -> Finished {
        self.finish_bounded(op, u64::MAX).await
    }

    /// Exec `req`, expect a new operation, wait for its receipt.
    pub async fn run(&mut self, req: ExecRequest) -> Finished {
        let op = req.op.clone();
        let ack = self.exec(req).await;
        assert_new(&ack);
        self.finish(&op).await
    }

    pub async fn write(&mut self, req: WriteRequest, bytes: &[u8]) -> Reply {
        let id = self.send(Request::Write(req)).await;
        self.send_data(id, bytes).await;
        self.reply_to(id).await
    }

    pub async fn read(&mut self, req: ReadRequest) -> Result<(FileData, Vec<u8>), GuestError> {
        let id = self.send(Request::Read(req)).await;
        match self.reply_to(id).await {
            Reply::FileData(header) => Ok((header, self.stream_of(id).await)),
            Reply::Error(error) => Err(error),
            other => panic!("expected file_data, got {other:?}"),
        }
    }

    pub async fn status(&mut self, op: &str) -> Reply {
        self.request(Request::Status { op: op.to_owned() }).await
    }

    /// Close the host's end of the channel; answers how serving ended.
    pub async fn close(self) -> Result<Exit, HandshakeError> {
        let Host {
            stream, serving, ..
        } = self;
        drop(stream);
        serving.await.unwrap()
    }

    /// Send `shutdown`; answers the reply and how serving ended.
    pub async fn shutdown(mut self) -> (Reply, Exit) {
        let reply = self.request(Request::Shutdown).await;
        let exit = self.serving.await.unwrap().unwrap();
        (reply, exit)
    }
}

pub fn write_request(
    op: &str,
    root: PathRoot,
    path: &str,
    bytes: &[u8],
    claimed: &[u8],
) -> WriteRequest {
    WriteRequest {
        op: op.to_owned(),
        args_sha256: sha256_hex(&[path.as_bytes(), bytes].concat()),
        root,
        path: path.to_owned(),
        sha256: sha256_hex(claimed),
        size: bytes.len() as u64,
        mode: 0o644,
    }
}

pub fn assert_new(ack: &Reply) {
    match ack {
        Reply::Accepted {
            state: OperationState::Accepted | OperationState::Running,
            existing: false,
        } => {}
        other => panic!("a new operation acks accepted or running, got {other:?}"),
    }
}

pub fn is_terminal(state: OperationState) -> bool {
    !matches!(
        state,
        OperationState::Accepted | OperationState::Running | OperationState::Unknown
    )
}

pub fn failed(exit: i32) -> OperationState {
    OperationState::Failed {
        exit: Some(exit),
        signal: None,
    }
}

pub fn path_exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}
