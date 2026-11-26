//! The control channel itself: handshake, protocol violations, shutdown
//! and channel loss, both of which end every process first.

mod support;

use std::time::{Duration, Instant};

use agentkit_sandbox_proto::guest::ReplyEnvelope;
use agentkit_sandbox_proto::guest::{GuestErrorCode, Reply, Request};
use agentkit_sandbox_proto::{
    Envelope, Frame, FrameDecoder, PROTOCOL_VERSION, encode_control, encode_data, parse_control,
};
use agentkit_sandboxd::{Exit, HandshakeError, ProcessMode};
use agentkit_sys::{KillProbe, kill_probe};
use support::{CHANNEL_TOKEN, GENERATION, Guest, assert_new, connect, exec_request, start};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const ENV: &str = "PATH=/usr/bin:/bin\n";

fn error_code(reply: &Reply) -> GuestErrorCode {
    match reply {
        Reply::Error(error) => error.code,
        other => panic!("expected an error, got {other:?}"),
    }
}

#[tokio::test]
async fn hello_with_another_generation_is_refused_and_closed() {
    let guest = Guest::new(ENV);
    let mut host = connect(&guest, ProcessMode::Embedded);
    let reply = host
        .request(Request::Hello {
            protocol: PROTOCOL_VERSION,
            generation: GENERATION + 1,
        })
        .await;
    assert_eq!(error_code(&reply), GuestErrorCode::GenerationMismatch);
    let outcome = host.serving.await.unwrap();
    assert!(matches!(
        outcome,
        Err(HandshakeError::GenerationMismatch {
            got: 8,
            expected: 7
        })
    ));
}

#[tokio::test]
async fn hello_with_another_protocol_is_refused() {
    let guest = Guest::new(ENV);
    let mut host = connect(&guest, ProcessMode::Embedded);
    let reply = host
        .request(Request::Hello {
            protocol: PROTOCOL_VERSION + 1,
            generation: GENERATION,
        })
        .await;
    assert_eq!(error_code(&reply), GuestErrorCode::ProtocolMismatch);
    assert!(matches!(
        host.serving.await.unwrap(),
        Err(HandshakeError::ProtocolMismatch { .. })
    ));
}

#[tokio::test]
async fn anything_before_hello_closes_the_channel() {
    let guest = Guest::new(ENV);
    let mut host = connect(&guest, ProcessMode::Embedded);
    host.send(Request::Usage).await;
    assert!(host.next_frame().await.is_none(), "nothing is served");
    assert!(matches!(
        host.serving.await.unwrap(),
        Err(HandshakeError::NotHello)
    ));
}

#[tokio::test]
async fn a_second_hello_and_unknown_messages_are_invalid_requests() {
    let guest = Guest::new(ENV);
    let mut host = start(&guest).await;
    let again = host
        .request(Request::Hello {
            protocol: PROTOCOL_VERSION,
            generation: GENERATION,
        })
        .await;
    assert_eq!(error_code(&again), GuestErrorCode::InvalidRequest);
    let unknown = br#"{"id":99,"body":{"type":"teleport"}}"#;
    let mut frame = Vec::new();
    frame.extend_from_slice(&((unknown.len() + 1) as u32).to_be_bytes());
    frame.push(1);
    frame.extend_from_slice(unknown);
    host.send_frame(&frame).await;
    assert_eq!(
        error_code(&host.reply_to(99).await),
        GuestErrorCode::InvalidRequest
    );
    assert_eq!(host.status("never").await, Reply::Unknown, "still serving");
}

#[tokio::test]
async fn data_for_no_open_stream_closes_the_channel() {
    let guest = Guest::new(ENV);
    let mut host = start(&guest).await;
    host.send_frame(&encode_data(42, 0, b"stray").unwrap())
        .await;
    assert_eq!(host.serving.await.unwrap().unwrap(), Exit::ChannelLost);
}

/// Start `sleep` that records its pid; answers the pid.
async fn running_sleep(host: &mut support::Host, guest: &Guest) -> i32 {
    let command = "printf $$ > pid.txt; exec sleep 3600";
    assert_new(&host.exec(exec_request("op-1", command)).await);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(text) = std::fs::read_to_string(guest.workspace.join("pid.txt"))
            && let Ok(pid) = text.parse()
        {
            return pid;
        }
        assert!(Instant::now() < deadline, "the command never started");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn shutdown_ends_every_process_then_answers_bye() {
    let guest = Guest::new(ENV);
    let mut host = start(&guest).await;
    let pid = running_sleep(&mut host, &guest).await;
    assert_eq!(kill_probe(pid), KillProbe::Alive);
    let (reply, exit) = host.shutdown().await;
    assert_eq!((reply, exit), (Reply::Bye, Exit::Shutdown));
    assert_eq!(
        kill_probe(pid),
        KillProbe::Gone,
        "the command is gone before bye"
    );
}

#[tokio::test]
async fn losing_the_channel_ends_every_process() {
    let guest = Guest::new(ENV);
    let mut host = start(&guest).await;
    let pid = running_sleep(&mut host, &guest).await;
    assert_eq!(host.close().await.unwrap(), Exit::ChannelLost);
    assert_eq!(kill_probe(pid), KillProbe::Gone);
}

/// One request and its control reply over a raw stream.
async fn exchange(stream: &mut tokio::net::UnixStream, id: u64, body: Request) -> Reply {
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
async fn run_keeps_dialing_until_the_host_listens() {
    let guest = Guest::new(ENV);
    let config = guest.config(ProcessMode::Embedded);
    let socket = config.control_socket.clone();
    let running = tokio::spawn(agentkit_sandboxd::run(config));
    tokio::time::sleep(Duration::from_millis(500)).await;
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let (mut stream, _) = listener.accept().await.unwrap();
    let hello = Request::Hello {
        protocol: PROTOCOL_VERSION,
        generation: GENERATION,
    };
    assert!(matches!(
        exchange(&mut stream, 1, hello).await,
        Reply::Hello { channel_token, .. } if channel_token == CHANNEL_TOKEN
    ));
    assert_eq!(
        exchange(&mut stream, 2, Request::Shutdown).await,
        Reply::Bye
    );
    assert_eq!(running.await.unwrap().unwrap(), Exit::Shutdown);
}
