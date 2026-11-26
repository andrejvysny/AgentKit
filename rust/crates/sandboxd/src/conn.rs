//! The control channel's two halves: a frame reader over the read half,
//! and one writer task that owns the write half, so frames from replies
//! running concurrently never interleave mid-frame. A reply and the data
//! frames of its stream are queued as one unit and written back to back.

use std::sync::Arc;

use agentkit_sandbox_proto::guest::{GuestError, GuestErrorCode, Reply};
use agentkit_sandbox_proto::{
    Envelope, Frame, FrameDecoder, FrameError, data_frames, encode_control, encode_data,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{Notify, mpsc, oneshot};
use tokio::task::JoinHandle;

/// Replies queued before the writer applies back-pressure.
const QUEUE_DEPTH: usize = 64;
/// Bytes read from the socket per call.
const READ_CHUNK: usize = 64 * 1024;

/// Decodes frames from the read half of the channel.
pub(crate) struct FrameReader<R> {
    inner: R,
    decoder: FrameDecoder,
    buf: Vec<u8>,
}

impl<R: AsyncRead + Unpin> FrameReader<R> {
    pub(crate) fn new(inner: R) -> Self {
        Self {
            inner,
            decoder: FrameDecoder::new(),
            buf: vec![0; READ_CHUNK],
        }
    }

    /// The next frame; `None` when the peer closed between frames. Cancel
    /// safe: a dropped call loses no bytes.
    pub(crate) async fn next(&mut self) -> Result<Option<Frame>, FrameError> {
        loop {
            if let Some(frame) = self.decoder.next_frame()? {
                return Ok(Some(frame));
            }
            let read = self.inner.read(&mut self.buf).await?;
            if read == 0 {
                if self.decoder.buffered() > 0 {
                    return Err(FrameError::Truncated);
                }
                return Ok(None);
            }
            self.decoder.push(&self.buf[..read]);
        }
    }
}

enum Outgoing {
    Frames(Vec<Vec<u8>>),
    Flush(oneshot::Sender<()>),
}

/// Queues replies for the writer task. Sends after the channel broke are
/// dropped: the serve loop notices the break and ends.
#[derive(Clone)]
pub(crate) struct Replier {
    tx: mpsc::Sender<Outgoing>,
}

impl Replier {
    /// Queue one control reply.
    pub(crate) async fn reply(&self, id: u64, reply: Reply) {
        let _ = self
            .tx
            .send(Outgoing::Frames(vec![control(id, &reply)]))
            .await;
    }

    /// Queue an error reply.
    pub(crate) async fn error(&self, id: u64, code: GuestErrorCode, message: impl Into<String>) {
        self.reply(id, error_reply(code, message)).await;
    }

    /// Queue a reply header followed by `bytes` as its data stream.
    pub(crate) async fn reply_with_data(&self, id: u64, header: Reply, bytes: &[u8]) {
        let mut frames = vec![control(id, &header)];
        for data in data_frames(id, bytes) {
            match encode_data(data.id, data.seq, &data.bytes) {
                Ok(frame) => frames.push(frame),
                // `data_frames` never exceeds the frame bound.
                Err(_) => return,
            }
        }
        let _ = self.tx.send(Outgoing::Frames(frames)).await;
    }

    /// Wait until everything queued before this call is written.
    pub(crate) async fn flush(&self) {
        let (done, wait) = oneshot::channel();
        if self.tx.send(Outgoing::Flush(done)).await.is_ok() {
            let _ = wait.await;
        }
    }
}

/// An error reply with `code` and a message for people.
pub(crate) fn error_reply(code: GuestErrorCode, message: impl Into<String>) -> Reply {
    Reply::Error(GuestError {
        code,
        message: message.into(),
    })
}

/// The control frame of `reply`. A reply that cannot be encoded (larger
/// than a control frame) becomes an `internal` error, which always fits.
fn control(id: u64, reply: &Reply) -> Vec<u8> {
    encode_control(&Envelope::new(id, reply)).unwrap_or_else(|_| {
        let fallback = error_reply(GuestErrorCode::Internal, "reply too large to encode");
        encode_control(&Envelope::new(id, fallback)).unwrap_or_default()
    })
}

/// Start the writer task over `write`. When a write fails it notifies
/// `broken` and stops; queued replies are then dropped.
pub(crate) fn spawn_writer<W>(write: W, broken: Arc<Notify>) -> (Replier, JoinHandle<()>)
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (tx, rx) = mpsc::channel(QUEUE_DEPTH);
    let task = tokio::spawn(write_loop(write, rx, broken));
    (Replier { tx }, task)
}

async fn write_loop<W: AsyncWrite + Unpin>(
    mut write: W,
    mut rx: mpsc::Receiver<Outgoing>,
    broken: Arc<Notify>,
) {
    while let Some(outgoing) = rx.recv().await {
        let frames = match outgoing {
            Outgoing::Frames(frames) => frames,
            Outgoing::Flush(done) => {
                let _ = done.send(());
                continue;
            }
        };
        let mut failed = false;
        for frame in &frames {
            if write.write_all(frame).await.is_err() {
                failed = true;
                break;
            }
        }
        if failed || write.flush().await.is_err() {
            broken.notify_one();
            return;
        }
    }
}
