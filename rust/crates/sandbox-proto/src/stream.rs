//! Byte streams over data frames.
//!
//! Some messages are followed by bytes: a `write` request (its `size`), a
//! `file_data` or `output_data` reply (its `len`). The bytes travel as data
//! frames carrying the message's envelope id, with sequence numbers 0, 1,
//! 2, … and at most [`DATA_MAX_BYTES`] each, and the stream always ends
//! with one empty data frame, also when it carries no bytes. The receiver
//! knows the total from the control message and refuses a stream that
//! is longer, shorter, out of order or for another id.

use crate::frame::{DATA_MAX_BYTES, DataFrame};

/// The data frames that carry `bytes` for request `id`, terminator
/// included.
pub fn data_frames(id: u64, bytes: &[u8]) -> impl Iterator<Item = DataFrame> + '_ {
    let chunks = bytes.chunks(DATA_MAX_BYTES).map(<[u8]>::to_vec);
    chunks
        .chain(std::iter::once(Vec::new()))
        .zip(0u64..)
        .map(move |(bytes, seq)| DataFrame { id, seq, bytes })
}

/// Why a data frame does not continue the expected stream.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StreamError {
    /// The frame belongs to another request.
    #[error("data frame for request {got}, expected {expected}")]
    WrongRequest {
        /// The stream's request id.
        expected: u64,
        /// The frame's request id.
        got: u64,
    },
    /// The frame's sequence is not the next one.
    #[error("data frame sequence {got}, expected {expected}")]
    OutOfOrder {
        /// The next sequence.
        expected: u64,
        /// The frame's sequence.
        got: u64,
    },
    /// The stream carries more bytes than announced.
    #[error("data stream is longer than the announced {expected} bytes")]
    TooLong {
        /// Announced total.
        expected: u64,
    },
    /// The stream ended before the announced total.
    #[error("data stream ended after {got} of {expected} bytes")]
    TooShort {
        /// Announced total.
        expected: u64,
        /// Bytes received.
        got: u64,
    },
    /// A frame arrived after the stream ended.
    #[error("data frame after the end of the stream")]
    Finished,
}

/// Collects one stream of announced length. Memory grows with the bytes
/// received, never past the announced total, so the caller bounds the
/// total before it creates the receiver.
#[derive(Debug)]
pub struct DataReceiver {
    id: u64,
    expected: u64,
    next_seq: u64,
    bytes: Vec<u8>,
    finished: bool,
}

impl DataReceiver {
    /// A receiver for request `id`'s stream of `expected` bytes.
    pub fn new(id: u64, expected: u64) -> Self {
        Self {
            id,
            expected,
            next_seq: 0,
            bytes: Vec::new(),
            finished: false,
        }
    }

    /// Take the next frame. Answers the whole content at the terminator,
    /// `None` before it. After the terminator or an error every call
    /// answers [`StreamError::Finished`].
    pub fn accept(&mut self, frame: DataFrame) -> Result<Option<Vec<u8>>, StreamError> {
        if self.finished {
            return Err(StreamError::Finished);
        }
        let result = self.take(frame);
        if result.is_err() {
            self.finished = true;
            self.bytes = Vec::new();
        }
        result
    }

    fn take(&mut self, frame: DataFrame) -> Result<Option<Vec<u8>>, StreamError> {
        if frame.id != self.id {
            return Err(StreamError::WrongRequest {
                expected: self.id,
                got: frame.id,
            });
        }
        if frame.seq != self.next_seq {
            return Err(StreamError::OutOfOrder {
                expected: self.next_seq,
                got: frame.seq,
            });
        }
        self.next_seq = self.next_seq.saturating_add(1);
        let got = self.bytes.len() as u64;
        if frame.bytes.is_empty() {
            self.finished = true;
            if got != self.expected {
                return Err(StreamError::TooShort {
                    expected: self.expected,
                    got,
                });
            }
            return Ok(Some(std::mem::take(&mut self.bytes)));
        }
        if got + frame.bytes.len() as u64 > self.expected {
            return Err(StreamError::TooLong {
                expected: self.expected,
            });
        }
        self.bytes.extend_from_slice(&frame.bytes);
        Ok(None)
    }
}

#[cfg(test)]
mod tests;
