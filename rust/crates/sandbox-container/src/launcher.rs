//! The launcher client: one connection per request, one control frame
//! each way, the whole exchange bounded by the launcher timeout. A fresh
//! connection per request keeps nothing shared between workspaces: a slow
//! `stop` of one never delays another's call.

use std::path::PathBuf;
use std::time::Duration;

use agentkit_sandbox_proto::launcher::{LauncherError, Reply, ReplyEnvelope, Request, Tier};
use agentkit_sandbox_proto::{Envelope, Frame, FrameDecoder, encode_control, parse_control};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use crate::token::ChannelToken;

/// Every request goes on its own connection, so its id is fixed.
const REQUEST_ID: u64 = 1;
/// Bytes read from the socket per call.
const READ_CHUNK: usize = 16 * 1024;
/// Longest container id kept from an `ensured` reply.
const CONTAINER_ID_MAX: usize = 128;

/// Why a launcher request did not complete.
#[derive(Debug)]
pub(crate) enum LauncherFailure {
    /// No answer: the socket refused, the exchange broke the protocol or
    /// ran out of time.
    Unreachable(String),
    /// The launcher answered with a refusal.
    Refused(LauncherError),
}

/// The answer to `ensure`.
#[derive(Debug)]
pub(crate) struct Ensured {
    pub(crate) container_id: String,
    pub(crate) token: ChannelToken,
}

/// The answer to `stop`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Stopped {
    pub(crate) proven: bool,
    pub(crate) reason: Option<String>,
}

/// Talks to the launcher over its unix socket.
#[derive(Debug, Clone)]
pub(crate) struct LauncherClient {
    socket: PathBuf,
    timeout: Duration,
}

impl LauncherClient {
    pub(crate) fn new(socket: PathBuf, timeout: Duration) -> Self {
        Self { socket, timeout }
    }

    /// Start, or confirm, the container of `generation`; answers its id
    /// and channel token.
    pub(crate) async fn ensure(
        &self,
        workspace_id: &str,
        generation: u64,
        image_digest: &str,
        tier: Tier,
    ) -> Result<Ensured, LauncherFailure> {
        let request = Request::Ensure {
            workspace_id: workspace_id.to_owned(),
            generation,
            image_digest: image_digest.to_owned(),
            tier,
        };
        match self.call(request).await? {
            Reply::Ensured {
                container_id,
                channel_token,
            } => {
                let token = ChannelToken::parse(channel_token).ok_or_else(|| {
                    LauncherFailure::Unreachable(
                        "the launcher answered an unusable channel token".to_owned(),
                    )
                })?;
                Ok(Ensured {
                    container_id: container_id.chars().take(CONTAINER_ID_MAX).collect(),
                    token,
                })
            }
            other => Err(unexpected("ensure", &other)),
        }
    }

    /// Stop and remove the container of `generation`.
    pub(crate) async fn stop(
        &self,
        workspace_id: &str,
        generation: u64,
        grace_ms: u64,
    ) -> Result<Stopped, LauncherFailure> {
        let request = Request::Stop {
            workspace_id: workspace_id.to_owned(),
            generation,
            grace_ms,
        };
        match self.call(request).await? {
            Reply::Stopped { proven, reason } => Ok(Stopped { proven, reason }),
            other => Err(unexpected("stop", &other)),
        }
    }

    /// Bytes of the workspace volume and of the home volume.
    pub(crate) async fn usage(&self, workspace_id: &str) -> Result<(u64, u64), LauncherFailure> {
        let request = Request::Usage {
            workspace_id: workspace_id.to_owned(),
        };
        match self.call(request).await? {
            Reply::Usage {
                workspace_bytes,
                home_bytes,
            } => Ok((workspace_bytes, home_bytes)),
            other => Err(unexpected("usage", &other)),
        }
    }

    /// `reset_home` or `destroy`, which answer `done`.
    pub(crate) async fn done(&self, request: Request) -> Result<(), LauncherFailure> {
        match self.call(request).await? {
            Reply::Done => Ok(()),
            other => Err(unexpected("a volume request", &other)),
        }
    }

    /// One exchange, bounded by the launcher timeout.
    async fn call(&self, request: Request) -> Result<Reply, LauncherFailure> {
        let millis = self.timeout.as_millis();
        match tokio::time::timeout(self.timeout, self.exchange(&request)).await {
            Ok(Ok(Reply::Error(error))) => Err(LauncherFailure::Refused(error)),
            Ok(outcome) => outcome,
            Err(_) => Err(LauncherFailure::Unreachable(format!(
                "no answer within {millis} ms"
            ))),
        }
    }

    async fn exchange(&self, request: &Request) -> Result<Reply, LauncherFailure> {
        let broken = |what: &str, error: &dyn std::fmt::Display| {
            LauncherFailure::Unreachable(format!("{what}: {error}"))
        };
        let mut stream = UnixStream::connect(&self.socket)
            .await
            .map_err(|error| broken("cannot connect", &error))?;
        let frame = encode_control(&Envelope::new(REQUEST_ID, request))
            .map_err(|error| broken("cannot encode the request", &error))?;
        stream
            .write_all(&frame)
            .await
            .map_err(|error| broken("cannot send the request", &error))?;
        read_reply(&mut stream).await
    }
}

/// The reply to [`REQUEST_ID`]; anything else on the connection is a
/// protocol fault.
async fn read_reply(stream: &mut UnixStream) -> Result<Reply, LauncherFailure> {
    let fault = |what: String| LauncherFailure::Unreachable(what);
    let mut decoder = FrameDecoder::new();
    let mut buf = vec![0; READ_CHUNK];
    loop {
        match decoder.next_frame() {
            Ok(Some(Frame::Control(json))) => {
                let envelope: ReplyEnvelope = parse_control(&json)
                    .map_err(|error| fault(format!("invalid reply: {error}")))?;
                if envelope.id != REQUEST_ID {
                    return Err(fault(format!("reply for request {}", envelope.id)));
                }
                return Ok(envelope.body);
            }
            Ok(Some(Frame::Data(_))) => return Err(fault("unexpected data frame".to_owned())),
            Ok(None) => {}
            Err(error) => return Err(fault(format!("invalid frame: {error}"))),
        }
        let read = stream
            .read(&mut buf)
            .await
            .map_err(|error| fault(format!("cannot read the reply: {error}")))?;
        if read == 0 {
            return Err(fault("closed before answering".to_owned()));
        }
        decoder.push(&buf[..read]);
    }
}

fn unexpected(what: &str, reply: &Reply) -> LauncherFailure {
    let name = match reply {
        Reply::Ensured { .. } => "ensured",
        Reply::Stopped { .. } => "stopped",
        Reply::Inspected { .. } => "inspected",
        Reply::Usage { .. } => "usage",
        Reply::Done => "done",
        Reply::Error(_) => "error",
    };
    LauncherFailure::Unreachable(format!("answered `{name}` to {what}"))
}
