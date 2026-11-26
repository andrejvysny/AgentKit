//! The guest channel of one machine: one control connection, a writer
//! task that owns the write half (frames of one request are written back
//! to back), and a reader task that decodes frames, checks each reply
//! against its request ([`Expect`]) and hands it to the waiting caller.
//!
//! Requests run concurrently and are matched by id. Data streams are
//! collected by the reader itself, bounded by the request before a byte is
//! stored, so a slow caller never holds the reader up. Anything that
//! breaks the protocol (an undecodable frame, a reply for no request, a
//! reply of the wrong kind, a malformed or oversized stream) closes the
//! channel; the machine is then unreachable for good.
//!
//! A caller that gives up (its timeout, or its future dropped) leaves its
//! slot behind once the request was queued, so a late reply is consumed
//! instead of read as a violation. A guest that leaves [`SLOTS_MAX`]
//! requests unanswered is treated as broken.

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use agentkit_sandbox_proto::guest::{Reply, ReplyEnvelope, Request};
use agentkit_sandbox_proto::{
    DataFrame, DataReceiver, Envelope, Frame, FrameDecoder, data_frames, encode_control,
    encode_data, parse_control,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::expect::{Expect, Verdict};

/// Requests waiting for a reply, answered or abandoned, at once.
pub(crate) const SLOTS_MAX: usize = 256;
/// Requests queued for the writer.
const QUEUE_DEPTH: usize = 64;
/// Bytes read from the socket per call.
const READ_CHUNK: usize = 64 * 1024;

/// A complete reply and the bytes of its data stream (empty without one).
#[derive(Debug)]
pub(crate) struct Response {
    pub(crate) reply: Reply,
    pub(crate) bytes: Vec<u8>,
}

/// Why a request got no reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ChannelError {
    /// The channel is closed; the text says why.
    Closed(String),
    /// No complete reply within the request's time.
    Timeout,
    /// The request does not fit one control frame; nothing was sent.
    TooLarge(String),
}

struct Slot {
    expect: Expect,
    tx: oneshot::Sender<Response>,
    stream: Option<(Reply, DataReceiver)>,
}

#[derive(Default)]
struct Pending {
    closed: Option<String>,
    slots: HashMap<u64, Slot>,
}

struct Shared {
    pending: Mutex<Pending>,
    cancel: CancellationToken,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Shared {
    /// Close once: the first reason sticks, every waiter is answered
    /// `Closed`, both tasks stop.
    fn shut(&self, reason: String) {
        let mut pending = lock(&self.pending);
        pending.closed.get_or_insert(reason);
        pending.slots.clear();
        drop(pending);
        self.cancel.cancel();
    }

    fn reason(&self) -> String {
        lock(&self.pending)
            .closed
            .clone()
            .unwrap_or_else(|| "the channel is closed".to_owned())
    }

    /// Route one frame; `Err` names the violation.
    fn on_frame(&self, frame: Frame) -> Result<(), String> {
        let mut pending = lock(&self.pending);
        match frame {
            Frame::Control(json) => {
                let envelope: ReplyEnvelope =
                    parse_control(&json).map_err(|error| format!("an invalid reply: {error}"))?;
                pending.on_reply(envelope)
            }
            Frame::Data(data) => pending.on_data(data),
        }
    }
}

impl Pending {
    fn on_reply(&mut self, envelope: ReplyEnvelope) -> Result<(), String> {
        let id = envelope.id;
        let slot = self
            .slots
            .get_mut(&id)
            .ok_or_else(|| format!("a reply to no request ({id})"))?;
        if slot.stream.is_some() {
            return Err(format!("a second reply to request {id}"));
        }
        match slot.expect.verdict(&envelope.body) {
            Verdict::Violation(why) => Err(format!("{why} (request {id})")),
            Verdict::Stream(len) => {
                slot.stream = Some((envelope.body, DataReceiver::new(id, len)));
                Ok(())
            }
            Verdict::Complete => {
                self.complete(id, envelope.body, Vec::new());
                Ok(())
            }
        }
    }

    fn on_data(&mut self, data: DataFrame) -> Result<(), String> {
        let id = data.id;
        let receiver = match self.slots.get_mut(&id) {
            Some(Slot {
                stream: Some((_, receiver)),
                ..
            }) => receiver,
            _ => return Err(format!("data for no open stream ({id})")),
        };
        match receiver.accept(data) {
            Err(error) => Err(format!("a broken data stream: {error}")),
            Ok(None) => Ok(()),
            Ok(Some(bytes)) => {
                let header = match self.slots.get_mut(&id).and_then(|slot| slot.stream.take()) {
                    Some((header, _)) => header,
                    None => return Err(format!("data for no open stream ({id})")),
                };
                self.complete(id, header, bytes);
                Ok(())
            }
        }
    }

    /// Hand the reply on; a caller that gave up no longer listens.
    fn complete(&mut self, id: u64, reply: Reply, bytes: Vec<u8>) {
        if let Some(slot) = self.slots.remove(&id) {
            let _ = slot.tx.send(Response { reply, bytes });
        }
    }
}

struct Outgoing {
    control: Vec<u8>,
    data: Option<(u64, Vec<u8>)>,
}

/// The host end of one guest channel. Dropping it closes the channel.
pub(crate) struct Channel {
    outgoing: mpsc::Sender<Outgoing>,
    shared: Arc<Shared>,
    next_id: AtomicU64,
}

impl Channel {
    /// Serve `stream`: start the reader and the writer.
    pub(crate) fn start(stream: UnixStream) -> Self {
        let (read, write) = stream.into_split();
        let shared = Arc::new(Shared {
            pending: Mutex::new(Pending::default()),
            cancel: CancellationToken::new(),
        });
        let (outgoing, queue) = mpsc::channel(QUEUE_DEPTH);
        tokio::spawn(read_loop(read, shared.clone()));
        tokio::spawn(write_loop(write, queue, shared.clone()));
        Self {
            outgoing,
            shared,
            next_id: AtomicU64::new(1),
        }
    }

    /// Send `body` (and `data` as its stream) and wait for the reply, all
    /// within `timeout`.
    pub(crate) async fn request(
        &self,
        body: &Request,
        data: Option<Vec<u8>>,
        expect: Expect,
        timeout: Duration,
    ) -> Result<Response, ChannelError> {
        let deadline = Instant::now() + timeout;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let control = encode_control(&Envelope::new(id, body))
            .map_err(|error| ChannelError::TooLarge(error.to_string()))?;
        let reply = self.register(id, expect)?;
        let message = Outgoing {
            control,
            data: data.map(|bytes| (id, bytes)),
        };
        match tokio::time::timeout_at(deadline, self.outgoing.send(message)).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => return Err(self.closed()),
            Err(_) => {
                // Never queued: no reply can come, so the slot goes.
                lock(&self.shared.pending).slots.remove(&id);
                return Err(ChannelError::Timeout);
            }
        }
        match tokio::time::timeout_at(deadline, reply).await {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(_)) => Err(self.closed()),
            Err(_) => Err(ChannelError::Timeout),
        }
    }

    fn register(
        &self,
        id: u64,
        expect: Expect,
    ) -> Result<oneshot::Receiver<Response>, ChannelError> {
        let mut pending = lock(&self.shared.pending);
        if let Some(reason) = &pending.closed {
            return Err(ChannelError::Closed(reason.clone()));
        }
        if pending.slots.len() >= SLOTS_MAX {
            drop(pending);
            let reason = format!("the guest left {SLOTS_MAX} requests unanswered");
            self.shared.shut(reason.clone());
            return Err(ChannelError::Closed(reason));
        }
        let (tx, rx) = oneshot::channel();
        let slot = Slot {
            expect,
            tx,
            stream: None,
        };
        pending.slots.insert(id, slot);
        Ok(rx)
    }

    fn closed(&self) -> ChannelError {
        ChannelError::Closed(self.shared.reason())
    }

    /// Close the channel for `reason` (a violation found above the frame
    /// level, or a stop). The first reason sticks.
    pub(crate) fn close(&self, reason: &str) {
        self.shared.shut(reason.to_owned());
    }

    /// Why the channel is closed, if it is.
    pub(crate) fn failure(&self) -> Option<String> {
        lock(&self.shared.pending).closed.clone()
    }
}

impl Drop for Channel {
    fn drop(&mut self) {
        self.shared.shut("the host dropped the channel".to_owned());
    }
}

async fn read_loop(mut read: OwnedReadHalf, shared: Arc<Shared>) {
    let reason = tokio::select! {
        reason = pump(&mut read, &shared) => reason,
        () = shared.cancel.cancelled() => return,
    };
    shared.shut(reason);
}

/// Decode and route frames until the channel breaks; answers why.
async fn pump(read: &mut OwnedReadHalf, shared: &Shared) -> String {
    let mut decoder = FrameDecoder::new();
    let mut buf = vec![0; READ_CHUNK];
    loop {
        loop {
            match decoder.next_frame() {
                Ok(Some(frame)) => {
                    if let Err(violation) = shared.on_frame(frame) {
                        return format!("the guest broke the protocol: {violation}");
                    }
                }
                Ok(None) => break,
                Err(error) => return format!("the guest broke the protocol: {error}"),
            }
        }
        match read.read(&mut buf).await {
            Ok(0) => return "the guest closed the channel".to_owned(),
            Ok(read) => decoder.push(&buf[..read]),
            Err(error) => return format!("reading the channel failed: {error}"),
        }
    }
}

async fn write_loop(
    mut write: OwnedWriteHalf,
    mut queue: mpsc::Receiver<Outgoing>,
    shared: Arc<Shared>,
) {
    let outcome = tokio::select! {
        outcome = drain_queue(&mut write, &mut queue) => outcome,
        () = shared.cancel.cancelled() => return,
    };
    if let Err(error) = outcome {
        shared.shut(format!("writing the channel failed: {error}"));
    }
}

/// Write each request's frames back to back until the queue closes.
async fn drain_queue(
    write: &mut OwnedWriteHalf,
    queue: &mut mpsc::Receiver<Outgoing>,
) -> io::Result<()> {
    while let Some(message) = queue.recv().await {
        write.write_all(&message.control).await?;
        if let Some((id, bytes)) = message.data {
            for frame in data_frames(id, &bytes) {
                let encoded =
                    encode_data(frame.id, frame.seq, &frame.bytes).map_err(io::Error::other)?;
                write.write_all(&encoded).await?;
            }
        }
        write.flush().await?;
    }
    Ok(())
}
