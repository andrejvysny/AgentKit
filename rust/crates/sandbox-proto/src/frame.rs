//! The frame codec shared by the guest channel and the launcher socket.
//!
//! A frame is a `u32` big-endian length of the rest of the frame, then one
//! kind byte, then the payload:
//!
//! - kind [`KIND_CONTROL`]: UTF-8 JSON text of one envelope, at most
//!   [`CONTROL_MAX_BYTES`];
//! - kind [`KIND_DATA`]: `u64` big-endian request id, `u64` big-endian
//!   sequence, then raw bytes, at most [`DATA_MAX_BYTES`].
//!
//! The length counts the kind byte, so a control frame's length is
//! `1 + json` and a data frame's is `1 + 16 + bytes`. Every length is
//! checked before anything is allocated for the body. Any [`FrameError`]
//! from decoding means the peer broke the protocol and the channel closes
//! (ADR 0039); [`FrameDecoder`] refuses further input after one.

use std::io::{self, Read, Write};

use serde::Serialize;
use serde::de::{DeserializeOwned, IgnoredAny};

/// Protocol version both ends send in `hello`.
pub const PROTOCOL_VERSION: u32 = 1;
/// Kind byte of a control frame (JSON envelope).
pub const KIND_CONTROL: u8 = 1;
/// Kind byte of a data frame (raw bytes of one request's stream).
pub const KIND_DATA: u8 = 2;
/// Bytes of the length prefix.
pub const LENGTH_BYTES: usize = 4;
/// Most JSON bytes in one control frame: 64 KiB.
pub const CONTROL_MAX_BYTES: usize = 64 * 1024;
/// Bytes of a data frame's header (request id and sequence).
pub const DATA_HEADER_BYTES: usize = 16;
/// Most payload bytes in one data frame, after its header: 1 MiB.
pub const DATA_MAX_BYTES: usize = 1024 * 1024;

const CONTROL_MAX_LEN: u64 = 1 + CONTROL_MAX_BYTES as u64;
const DATA_MIN_LEN: u64 = 1 + DATA_HEADER_BYTES as u64;
const DATA_MAX_LEN: u64 = DATA_MIN_LEN + DATA_MAX_BYTES as u64;

/// Why bytes are not a valid frame, or a frame cannot be written.
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    /// The length prefix is 0: there is no kind byte.
    #[error("frame length is zero")]
    EmptyFrame,
    /// The kind byte is neither control nor data.
    #[error("unknown frame kind {0}")]
    UnknownKind(u8),
    /// The frame length is over the maximum for its kind (or for any kind,
    /// when the kind is not known yet).
    #[error("frame length {len} is over the maximum {max}")]
    TooLarge {
        /// Length of the rest of the frame, kind byte included.
        len: u64,
        /// Largest allowed length.
        max: u64,
    },
    /// A data frame is shorter than its 16-byte header.
    #[error("data frame length {len} is shorter than its header")]
    DataTooShort {
        /// Length of the rest of the frame, kind byte included.
        len: u64,
    },
    /// A control payload is not UTF-8.
    #[error("control frame is not UTF-8")]
    InvalidUtf8,
    /// A control payload is not JSON.
    #[error("control frame is not JSON: {0}")]
    InvalidJson(#[source] serde_json::Error),
    /// A control payload is JSON but not the expected message.
    #[error("control frame is not a valid message: {0}")]
    InvalidMessage(#[source] serde_json::Error),
    /// A message cannot be serialized.
    #[error("message cannot be serialized: {0}")]
    Serialize(#[source] serde_json::Error),
    /// The stream ended inside a frame.
    #[error("stream ended inside a frame")]
    Truncated,
    /// The decoder already rejected a frame; the channel must close.
    #[error("decoder already rejected a frame")]
    Poisoned,
    /// Reading or writing the stream failed.
    #[error("frame i/o failed: {0}")]
    Io(#[from] io::Error),
}

/// A data frame: one chunk of the byte stream of request `id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataFrame {
    /// Id of the request whose stream this chunk belongs to.
    pub id: u64,
    /// Index of this chunk in its stream, from 0. Not a log offset.
    pub seq: u64,
    /// The chunk; empty for the frame that ends a stream.
    pub bytes: Vec<u8>,
}

/// One decoded frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// JSON text of one envelope. A decoded one is valid JSON; read it with
    /// [`parse_control`].
    Control(String),
    /// A chunk of a request's byte stream.
    Data(DataFrame),
}

impl Frame {
    /// The frame's wire bytes, length prefix included.
    pub fn encode(&self) -> Result<Vec<u8>, FrameError> {
        match self {
            Frame::Control(json) => encode_control_text(json),
            Frame::Data(data) => encode_data(data.id, data.seq, &data.bytes),
        }
    }
}

/// Serialize `message` (an envelope) into a control frame.
pub fn encode_control<T: Serialize>(message: &T) -> Result<Vec<u8>, FrameError> {
    let json = serde_json::to_string(message).map_err(FrameError::Serialize)?;
    encode_control_text(&json)
}

fn encode_control_text(json: &str) -> Result<Vec<u8>, FrameError> {
    let len = 1 + json.len() as u64;
    if len > CONTROL_MAX_LEN {
        return Err(FrameError::TooLarge {
            len,
            max: CONTROL_MAX_LEN,
        });
    }
    let mut out = Vec::with_capacity(LENGTH_BYTES + 1 + json.len());
    push_header(&mut out, len, KIND_CONTROL);
    out.extend_from_slice(json.as_bytes());
    Ok(out)
}

/// Encode one data frame of request `id`'s stream.
pub fn encode_data(id: u64, seq: u64, bytes: &[u8]) -> Result<Vec<u8>, FrameError> {
    let len = DATA_MIN_LEN + bytes.len() as u64;
    if len > DATA_MAX_LEN {
        return Err(FrameError::TooLarge {
            len,
            max: DATA_MAX_LEN,
        });
    }
    let mut out = Vec::with_capacity(LENGTH_BYTES + DATA_MIN_LEN as usize + bytes.len());
    push_header(&mut out, len, KIND_DATA);
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(&seq.to_be_bytes());
    out.extend_from_slice(bytes);
    Ok(out)
}

fn push_header(out: &mut Vec<u8>, len: u64, kind: u8) {
    // Callers bound `len` by the kind's maximum, which fits in a u32.
    let len = u32::try_from(len).unwrap_or(u32::MAX);
    out.extend_from_slice(&len.to_be_bytes());
    out.push(kind);
}

/// Parse the JSON text of a control frame as message `T`.
pub fn parse_control<T: DeserializeOwned>(json: &str) -> Result<T, FrameError> {
    serde_json::from_str(json).map_err(FrameError::InvalidMessage)
}

/// Check a length prefix on its own: not zero, not over any kind's maximum.
fn check_len(len: u32) -> Result<(), FrameError> {
    let len = u64::from(len);
    if len == 0 {
        return Err(FrameError::EmptyFrame);
    }
    if len > DATA_MAX_LEN {
        return Err(FrameError::TooLarge {
            len,
            max: DATA_MAX_LEN,
        });
    }
    Ok(())
}

/// Check a length prefix against its kind.
fn check_kind(len: u32, kind: u8) -> Result<(), FrameError> {
    let len = u64::from(len);
    let max = match kind {
        KIND_CONTROL => CONTROL_MAX_LEN,
        KIND_DATA if len < DATA_MIN_LEN => return Err(FrameError::DataTooShort { len }),
        KIND_DATA => DATA_MAX_LEN,
        other => return Err(FrameError::UnknownKind(other)),
    };
    if len > max {
        return Err(FrameError::TooLarge { len, max });
    }
    Ok(())
}

/// Decode a body (the bytes after the kind byte) of a checked kind.
fn decode_body(kind: u8, body: &[u8]) -> Result<Frame, FrameError> {
    if kind == KIND_CONTROL {
        let text = std::str::from_utf8(body).map_err(|_| FrameError::InvalidUtf8)?;
        serde_json::from_str::<IgnoredAny>(text).map_err(FrameError::InvalidJson)?;
        return Ok(Frame::Control(text.to_owned()));
    }
    let too_short = || FrameError::DataTooShort {
        len: 1 + body.len() as u64,
    };
    let (id, rest) = body.split_first_chunk::<8>().ok_or_else(too_short)?;
    let (seq, bytes) = rest.split_first_chunk::<8>().ok_or_else(too_short)?;
    Ok(Frame::Data(DataFrame {
        id: u64::from_be_bytes(*id),
        seq: u64::from_be_bytes(*seq),
        bytes: bytes.to_vec(),
    }))
}

/// Incremental decoder for callers that read the stream themselves (an
/// async socket). [`push`](FrameDecoder::push) what was read, then call
/// [`next_frame`](FrameDecoder::next_frame) until it answers `None`; the
/// buffer then never holds more than one frame plus one read.
#[derive(Debug, Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
    /// Bytes at the front of `buf` already returned as frames. Dropped once
    /// per push, not once per frame, so many small frames in one read cost
    /// linear time.
    start: usize,
    poisoned: bool,
}

impl FrameDecoder {
    /// An empty decoder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append bytes read from the stream.
    pub fn push(&mut self, bytes: &[u8]) {
        if !self.poisoned {
            self.buf.drain(..self.start);
            self.start = 0;
            self.buf.extend_from_slice(bytes);
        }
    }

    /// Bytes buffered that do not form a whole frame yet. Non-zero at the
    /// end of the stream means it ended inside a frame.
    pub fn buffered(&self) -> usize {
        self.buf.len() - self.start
    }

    /// The next whole frame, `None` when more bytes are needed. After an
    /// error every call answers [`FrameError::Poisoned`].
    pub fn next_frame(&mut self) -> Result<Option<Frame>, FrameError> {
        if self.poisoned {
            return Err(FrameError::Poisoned);
        }
        let result = self.decode_next();
        if result.is_err() {
            self.poisoned = true;
            self.buf = Vec::new();
            self.start = 0;
        }
        result
    }

    fn decode_next(&mut self) -> Result<Option<Frame>, FrameError> {
        let rest = &self.buf[self.start..];
        let Some(prefix) = rest.first_chunk::<LENGTH_BYTES>() else {
            return Ok(None);
        };
        let len = u32::from_be_bytes(*prefix);
        check_len(len)?;
        let Some(&kind) = rest.get(LENGTH_BYTES) else {
            return Ok(None);
        };
        check_kind(len, kind)?;
        let end = LENGTH_BYTES + len as usize;
        let Some(body) = rest.get(LENGTH_BYTES + 1..end) else {
            return Ok(None);
        };
        let frame = decode_body(kind, body)?;
        self.start += end;
        if self.start == self.buf.len() {
            self.buf.clear();
            self.start = 0;
        }
        Ok(Some(frame))
    }
}

/// Read one frame from a blocking stream. `Ok(None)` when the stream ends
/// cleanly between frames; [`FrameError::Truncated`] when it ends inside
/// one.
pub fn read_frame<R: Read>(reader: &mut R) -> Result<Option<Frame>, FrameError> {
    let mut prefix = [0u8; LENGTH_BYTES];
    let mut filled = 0;
    while filled < LENGTH_BYTES {
        match reader.read(&mut prefix[filled..]) {
            Ok(0) if filled == 0 => return Ok(None),
            Ok(0) => return Err(FrameError::Truncated),
            Ok(n) => filled += n,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(FrameError::Io(error)),
        }
    }
    let len = u32::from_be_bytes(prefix);
    check_len(len)?;
    let mut kind = [0u8; 1];
    read_exact(reader, &mut kind)?;
    check_kind(len, kind[0])?;
    let mut body = vec![0u8; len as usize - 1];
    read_exact(reader, &mut body)?;
    decode_body(kind[0], &body).map(Some)
}

fn read_exact<R: Read>(reader: &mut R, buf: &mut [u8]) -> Result<(), FrameError> {
    reader.read_exact(buf).map_err(|error| {
        if error.kind() == io::ErrorKind::UnexpectedEof {
            FrameError::Truncated
        } else {
            FrameError::Io(error)
        }
    })
}

/// Write one frame to a blocking stream and flush it.
pub fn write_frame<W: Write>(writer: &mut W, frame: &Frame) -> Result<(), FrameError> {
    writer.write_all(&frame.encode()?)?;
    writer.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests;
