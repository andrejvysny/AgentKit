//! A scripted guest: dials the machine's control socket like the
//! supervisor and answers each request with whatever its script says,
//! including replies that break the protocol.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use agentkit_sandbox_proto::guest::{Reply, Request, RequestEnvelope};
use agentkit_sandbox_proto::{
    DataFrame, Envelope, Frame, FrameDecoder, PROTOCOL_VERSION, data_frames, encode_control,
    encode_data, parse_control,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use super::launcher::{Start, Starter};

/// What the guest does about one request.
pub enum Action {
    /// Write these encoded frames.
    Send(Vec<Vec<u8>>),
    /// Answer nothing.
    Silent,
    /// Close the channel.
    Close,
}

/// What a guest knows about its machine.
#[derive(Debug, Clone)]
pub struct Peer {
    pub generation: u64,
    /// The container's channel token (its `AGENTKIT_CHANNEL_TOKEN`).
    pub token: String,
}

/// The script: request id, the guest's machine and request to an action.
pub type Script = Arc<dyn Fn(u64, &Peer, &Request) -> Action + Send + Sync>;

/// A control frame carrying `reply`.
pub fn reply(id: u64, reply: Reply) -> Vec<u8> {
    encode_control(&Envelope::new(id, reply)).unwrap()
}

/// `reply` followed by `bytes` as its data stream.
pub fn reply_with(id: u64, header: Reply, bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut frames = vec![reply(id, header)];
    for DataFrame { id, seq, bytes } in data_frames(id, bytes) {
        frames.push(encode_data(id, seq, &bytes).unwrap());
    }
    frames
}

/// The honest answer to `hello` for `peer` and `digest`.
pub fn hello(id: u64, peer: &Peer, digest: &str) -> Action {
    Action::Send(vec![reply(
        id,
        Reply::Hello {
            protocol: PROTOCOL_VERSION,
            generation: peer.generation,
            image_digest: digest.to_owned(),
            channel_token: peer.token.clone(),
        },
    )])
}

/// Starts a scripted guest for each machine.
pub fn starter(script: Script) -> Starter {
    Arc::new(move |start: Start| {
        let script = script.clone();
        tokio::spawn(async move { run(start, script).await })
    })
}

/// Starts nothing: a container whose supervisor never dials.
pub fn silent_starter() -> Starter {
    Arc::new(|_start: Start| tokio::spawn(std::future::pending()))
}

pub async fn dial(path: &Path) -> Option<UnixStream> {
    for _ in 0..250 {
        if let Ok(stream) = UnixStream::connect(path).await {
            return Some(stream);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    None
}

async fn run(start: Start, script: Script) {
    let Some(stream) = dial(&start.dir.join("control.sock")).await else {
        return;
    };
    let peer = Peer {
        generation: start.generation,
        token: start.channel_token,
    };
    serve(stream, &peer, &script).await;
}

/// Answer requests on `stream` by `script` until it closes.
pub async fn serve(mut stream: UnixStream, peer: &Peer, script: &Script) {
    let mut decoder = FrameDecoder::new();
    let mut buf = vec![0; 64 * 1024];
    // A write's reply waits for the end of its data stream.
    let mut writes: Vec<(u64, Request)> = Vec::new();
    loop {
        while let Ok(Some(frame)) = decoder.next_frame() {
            let action = match frame {
                Frame::Control(json) => {
                    let Ok(envelope) = parse_control::<RequestEnvelope>(&json) else {
                        return;
                    };
                    if matches!(envelope.body, Request::Write(_)) {
                        writes.push((envelope.id, envelope.body));
                        continue;
                    }
                    script(envelope.id, peer, &envelope.body)
                }
                Frame::Data(data) if data.bytes.is_empty() => {
                    let Some(at) = writes.iter().position(|(id, _)| *id == data.id) else {
                        return;
                    };
                    let (id, body) = writes.remove(at);
                    script(id, peer, &body)
                }
                Frame::Data(_) => continue,
            };
            match action {
                Action::Send(frames) => {
                    for frame in frames {
                        if stream.write_all(&frame).await.is_err() {
                            return;
                        }
                    }
                }
                Action::Silent => {}
                Action::Close => return,
            }
        }
        match stream.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(read) => decoder.push(&buf[..read]),
        }
    }
}
