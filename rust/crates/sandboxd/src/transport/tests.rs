use std::path::PathBuf;

use agentkit_sandbox_proto::guest::{Reply, ReplyEnvelope, Request};
use agentkit_sandbox_proto::{
    Envelope, Frame, FrameDecoder, PROTOCOL_VERSION, encode_control, parse_control,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

use super::*;
use crate::config::{Config, ProcessMode};
use crate::error::HandshakeError;
use crate::server::{Exit, run_session};
use crate::shim::Upstream;

const PORT: u32 = 1024;
const TOKEN: &str = "vsock-channel-token";

/// Stands in for the vsock listener where vsock does not exist.
impl ControlListener for UnixListener {
    type Stream = UnixStream;

    async fn accept_stream(&self) -> io::Result<UnixStream> {
        self.accept().await.map(|(stream, _)| stream)
    }
}

fn config(dir: &std::path::Path) -> Config {
    let workspace = dir.join("workspace");
    let home = dir.join("home");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::create_dir(&home).unwrap();
    std::fs::write(dir.join("environment"), "PATH=/usr/bin:/bin\n").unwrap();
    Config {
        generation: 7,
        image_digest: "sha256:vm-image".to_owned(),
        channel_token: TOKEN.to_owned(),
        control_socket: dir.join("unused-control.sock"),
        egress_socket: dir.join("egress.sock"),
        workspace_root: workspace,
        home_root: home,
        env_file: dir.join("environment"),
        shell: PathBuf::from("/bin/bash"),
        shim_addr: "127.0.0.1:0".parse().unwrap(),
        mode: ProcessMode::Embedded,
    }
}

/// One request and its control reply.
async fn exchange(stream: &mut UnixStream, id: u64, body: Request) -> Reply {
    let frame = encode_control(&Envelope::new(id, body)).unwrap();
    stream.write_all(&frame).await.unwrap();
    let mut decoder = FrameDecoder::new();
    let mut buf = vec![0; 4096];
    loop {
        if let Some(Frame::Control(json)) = decoder.next_frame().unwrap() {
            let reply: ReplyEnvelope = parse_control(&json).unwrap();
            assert_eq!(reply.id, id);
            return reply.body;
        }
        let read = stream.read(&mut buf).await.unwrap();
        assert!(read > 0, "the guest closed the channel");
        decoder.push(&buf[..read]);
    }
}

#[tokio::test]
async fn the_accepted_connection_runs_the_same_session_and_is_the_only_one() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("control.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let config = config(dir.path());
    let control = accept_once(listener, PORT, Duration::from_secs(10));
    let upstream = Upstream::Socket(config.egress_socket.clone());
    let running = tokio::spawn(run_session(config, upstream, control));
    let mut host = UnixStream::connect(&path).await.unwrap();
    let hello = Request::Hello {
        protocol: PROTOCOL_VERSION,
        generation: 7,
    };
    assert_eq!(
        exchange(&mut host, 1, hello).await,
        Reply::Hello {
            protocol: PROTOCOL_VERSION,
            generation: 7,
            image_digest: "sha256:vm-image".to_owned(),
            channel_token: TOKEN.to_owned(),
        }
    );
    assert!(
        UnixStream::connect(&path).await.is_err(),
        "the listener closed after its one connection"
    );
    assert_eq!(exchange(&mut host, 2, Request::Shutdown).await, Reply::Bye);
    assert_eq!(running.await.unwrap().unwrap(), Exit::Shutdown);
}

#[tokio::test]
async fn no_connection_within_the_window_is_a_startup_error() {
    let dir = tempfile::tempdir().unwrap();
    let listener = UnixListener::bind(dir.path().join("control.sock")).unwrap();
    let outcome = accept_once(listener, PORT, Duration::from_millis(200)).await;
    match outcome {
        Err(StartupError::Accept { port, source, .. }) => {
            assert_eq!(port, PORT);
            assert_eq!(source.kind(), io::ErrorKind::TimedOut);
        }
        other => panic!("expected an accept error, got {other:?}"),
    }
}

#[tokio::test]
async fn anything_before_hello_ends_the_session_like_a_dialed_one() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("control.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let config = config(dir.path());
    let control = accept_once(listener, PORT, Duration::from_secs(10));
    let upstream = Upstream::Socket(config.egress_socket.clone());
    let running = tokio::spawn(run_session(config, upstream, control));
    let mut host = UnixStream::connect(&path).await.unwrap();
    let frame = encode_control(&Envelope::new(1, Request::Usage)).unwrap();
    host.write_all(&frame).await.unwrap();
    let mut buf = [0; 64];
    let read = host.read(&mut buf).await.unwrap_or(0);
    assert_eq!(read, 0, "nothing is served");
    assert!(matches!(
        running.await.unwrap(),
        Err(StartupError::Handshake(HandshakeError::NotHello))
    ));
}
