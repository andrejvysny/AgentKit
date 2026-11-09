//! Bounded, versioned wire protocol (§6.4, S16).
//!
//! A frame is `[u32 LE length][UTF-8 JSON]`. The JSON is
//! `{"header":…,"body":…}` with `deny_unknown_fields` everywhere; unknown
//! fields, unknown operations, unsupported versions, oversized frames,
//! stale generations and expired deadlines all fail closed. There is no
//! hand-written delimiter protocol and no best-effort recovery: a connection
//! that sends a malformed frame is terminated by the caller.
//!
//! Authenticated host→worker messages carry the authoritative tenant binding
//! inside the body (grant or control envelope); the frame header never
//! substitutes for it (S02).

use crate::error::{SandboxError, SandboxErrorCode};
use crate::ids::{ArtifactDigest, ExecutionId, InstanceGeneration, RequestDigest, SandboxId};
use crate::limits::MAX_FRAME_BYTES;
use serde::{Deserialize, Serialize};

/// Errors raised while framing/parsing. All map to
/// `sandbox_protocol_incompatible` at the service layer.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ProtocolError {
    #[error("frame too large: {0} bytes (max {MAX_FRAME_BYTES})")]
    FrameTooLarge(u64),
    #[error("frame truncated: header complete but body incomplete")]
    Truncated,
    #[error("malformed frame json: {0}")]
    Malformed(String),
    #[error("payload_len {declared} does not match actual body length {actual}")]
    LengthMismatch { declared: u64, actual: u64 },
    #[error("payload_digest mismatch")]
    DigestMismatch,
    #[error("unsupported protocol version {0}")]
    UnsupportedVersion(u32),
    #[error("unknown operation {0:?}")]
    UnknownOperation(String),
    #[error("duplicate request_id with conflicting payload")]
    DuplicateConflict,
    #[error("sequence regression: got {got}, expected >= {expected}")]
    SequenceRegression { got: u64, expected: u64 },
    #[error("frame deadline expired")]
    DeadlineExpired,
    #[error("header field invalid: {0}")]
    InvalidHeader(&'static str),
}

impl From<ProtocolError> for SandboxError {
    fn from(e: ProtocolError) -> Self {
        SandboxError::new(SandboxErrorCode::ProtocolIncompatible, e.to_string())
    }
}

/// The frame envelope (§6.4). Every field is mandatory-or-explicitly-optional;
/// unknown fields are rejected at the serde layer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrameHeader {
    pub protocol_version: u32,
    /// Unique per connection and direction; duplicate-suppression key.
    pub request_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandbox_id: Option<SandboxId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance_generation: Option<InstanceGeneration>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_id: Option<ExecutionId>,
    /// Exact operation name; must match the body tag.
    pub operation: String,
    /// Monotonic per direction per connection; regressions rejected.
    pub sequence: u64,
    /// Absolute unix-ms deadline after which the frame must not be acted on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deadline_ms: Option<u64>,
    /// Byte length of the serialized body (validated on decode).
    pub payload_len: u64,
    /// Present when moving bytes or when the sender chose to bind identity.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload_digest: Option<ArtifactDigest>,
}

/// A wire frame: validated header + opaque JSON body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireFrame {
    pub header: FrameHeader,
    pub body: serde_json::Value,
}

/// Validate structural header invariants independent of the body.
pub fn validate_header(header: &FrameHeader, now_ms: u64) -> Result<(), ProtocolError> {
    if header.protocol_version != crate::PROTOCOL_VERSION {
        return Err(ProtocolError::UnsupportedVersion(header.protocol_version));
    }
    if header.request_id.is_empty()
        || header.request_id.len() > 128
        || !header
            .request_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err(ProtocolError::InvalidHeader("request_id"));
    }
    if header.operation.is_empty() || header.operation.len() > 64 {
        return Err(ProtocolError::InvalidHeader("operation"));
    }
    if let Some(g) = header.instance_generation
        && g == InstanceGeneration(0)
    {
        return Err(ProtocolError::InvalidHeader("instance_generation"));
    }
    if let Some(d) = header.deadline_ms
        && d <= now_ms
    {
        return Err(ProtocolError::DeadlineExpired);
    }
    Ok(())
}

/// Encode a frame: `[u32 LE len][json]`. Computes `payload_len` and, when
/// `digest_body` is set, the body digest. Fails if the frame exceeds
/// `MAX_FRAME_BYTES`.
pub fn encode_frame<T: Serialize>(
    mut header: FrameHeader,
    body: &T,
    digest_body: bool,
) -> Result<Vec<u8>, ProtocolError> {
    let body_value =
        serde_json::to_value(body).map_err(|e| ProtocolError::Malformed(e.to_string()))?;
    let body_bytes =
        serde_json::to_vec(&body_value).map_err(|e| ProtocolError::Malformed(e.to_string()))?;
    header.payload_len = body_bytes.len() as u64;
    header.payload_digest = if digest_body {
        Some(ArtifactDigest::of_bytes(&body_bytes))
    } else {
        None
    };
    let frame = WireFrame {
        header,
        body: body_value,
    };
    let json = serde_json::to_vec(&frame).map_err(|e| ProtocolError::Malformed(e.to_string()))?;
    if json.len() > MAX_FRAME_BYTES {
        return Err(ProtocolError::FrameTooLarge(json.len() as u64));
    }
    let mut out = Vec::with_capacity(json.len() + 4);
    out.extend_from_slice(&(json.len() as u32).to_le_bytes());
    out.extend_from_slice(&json);
    Ok(out)
}

/// Incremental frame reader for any byte transport (UDS, vsock, tests).
/// Feed bytes; pop whole frames in order. A single bad frame is returned as
/// `Err` and the decoder must be discarded (connections terminate on parse
/// errors — no resynchronization inside a bad stream).
pub struct FrameDecoder {
    buf: Vec<u8>,
    max_bytes: usize,
}

impl FrameDecoder {
    #[must_use]
    pub fn new() -> Self {
        Self {
            buf: Vec::new(),
            max_bytes: MAX_FRAME_BYTES,
        }
    }
    /// Override for tests only; production uses the default bound.
    #[must_use]
    pub fn with_max(mut self, max_bytes: usize) -> Self {
        self.max_bytes = max_bytes;
        self
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Try to pop the next complete frame.
    pub fn next_frame(&mut self, now_ms: u64) -> Result<Option<WireFrame>, ProtocolError> {
        if self.buf.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_le_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]) as u64;
        if len > self.max_bytes as u64 {
            return Err(ProtocolError::FrameTooLarge(len));
        }
        if self.buf.len() < 4 + len as usize {
            return Ok(None);
        }
        let json = &self.buf[4..4 + len as usize];
        let frame: WireFrame = serde_json::from_slice(json)
            .map_err(|e| ProtocolError::Malformed(truncate_msg(&e.to_string())))?;
        self.buf.drain(..4 + len as usize);
        validate_header(&frame.header, now_ms)?;
        // payload_len must describe the actual serialized body we received:
        // re-serialize compactly and compare lengths.
        let actual = serde_json::to_vec(&frame.body)
            .map_err(|e| ProtocolError::Malformed(e.to_string()))?
            .len() as u64;
        if frame.header.payload_len != actual {
            return Err(ProtocolError::LengthMismatch {
                declared: frame.header.payload_len,
                actual,
            });
        }
        if let Some(expected) = &frame.header.payload_digest {
            let got = ArtifactDigest::of_bytes(
                &serde_json::to_vec(&frame.body)
                    .map_err(|e| ProtocolError::Malformed(e.to_string()))?,
            );
            if &got != expected {
                return Err(ProtocolError::DigestMismatch);
            }
        }
        Ok(Some(frame))
    }

    /// Bytes currently buffered (for capacity guards on idle connections).
    #[must_use]
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }
}

impl Default for FrameDecoder {
    fn default() -> Self {
        Self::new()
    }
}

fn truncate_msg(s: &str) -> String {
    const N: usize = 200;
    if s.len() <= N {
        s.to_string()
    } else {
        let mut end = N;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &s[..end])
    }
}

/// Request id + digest duplicate suppression (S07, S16). Bounded: evicting
/// the oldest entry past `MAX_DUPLICATE_TABLE` is safe because a replayed
/// request older than the table is refused by the deadline/generation checks,
/// not by this table.
#[derive(Debug, Default)]
pub struct DuplicateTable {
    seen: std::collections::VecDeque<(String, RequestDigest)>,
    index: std::collections::HashMap<String, RequestDigest>,
}

/// Outcome of checking a request against the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DuplicateOutcome {
    /// First time seeing this request id.
    New,
    /// Same id, same digest: replay the recorded answer, do not re-run.
    Replay,
    /// Same id, different digest: protocol violation.
    Conflict,
}

impl DuplicateTable {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn check(&mut self, request_id: &str, digest: &RequestDigest) -> DuplicateOutcome {
        if let Some(prev) = self.index.get(request_id) {
            if prev == digest {
                return DuplicateOutcome::Replay;
            }
            return DuplicateOutcome::Conflict;
        }
        DuplicateOutcome::New
    }

    pub fn record(&mut self, request_id: impl Into<String>, digest: RequestDigest) {
        let id: String = request_id.into();
        if self.index.insert(id.clone(), digest.clone()).is_none() {
            self.seen.push_back((id, digest));
        }
        while self.seen.len() > crate::limits::MAX_DUPLICATE_TABLE {
            if let Some((old_id, _)) = self.seen.pop_front() {
                self.index.remove(&old_id);
            }
        }
    }
}

/// Monotonic sequence checker per connection direction.
#[derive(Debug, Default)]
pub struct SequenceGuard {
    next_expected: u64,
    started: bool,
}

impl SequenceGuard {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
    pub fn accept(&mut self, sequence: u64) -> Result<(), ProtocolError> {
        if self.started && sequence < self.next_expected {
            return Err(ProtocolError::SequenceRegression {
                got: sequence,
                expected: self.next_expected,
            });
        }
        self.next_expected = sequence + 1;
        self.started = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::InstanceGeneration;

    fn header(op: &str) -> FrameHeader {
        FrameHeader {
            protocol_version: crate::PROTOCOL_VERSION,
            request_id: "req-1".into(),
            sandbox_id: Some(SandboxId::new("sb1").unwrap()),
            instance_generation: Some(InstanceGeneration(2)),
            execution_id: None,
            operation: op.into(),
            sequence: 1,
            deadline_ms: None,
            payload_len: 0,
            payload_digest: None,
        }
    }

    #[test]
    fn roundtrip_and_digest() {
        let bytes = encode_frame(header("inspect"), &serde_json::json!({"a":1}), true).unwrap();
        let mut d = FrameDecoder::new();
        d.push(&bytes);
        let f = d.next_frame(0).unwrap().unwrap();
        assert_eq!(f.header.operation, "inspect");
        assert_eq!(f.body, serde_json::json!({"a":1}));
        assert!(d.next_frame(0).unwrap().is_none());
    }

    #[test]
    fn rejects_bad_versions_and_shapes() {
        let mut h = header("inspect");
        h.protocol_version = 7;
        let bytes = encode_frame(h.clone(), &serde_json::json!({}), false).unwrap();
        let mut d = FrameDecoder::new();
        d.push(&bytes);
        assert_eq!(d.next_frame(0), Err(ProtocolError::UnsupportedVersion(7)));

        let mut d = FrameDecoder::new().with_max(64);
        d.push(&[9, 0, 0, 0]); // 9 bytes frame within cap — feed json
        d.push(b"not json!!!");
        assert!(matches!(d.next_frame(0), Err(ProtocolError::Malformed(_))));
    }

    #[test]
    fn rejects_oversized_frames() {
        let mut d = FrameDecoder::new();
        d.push(&(MAX_FRAME_BYTES as u32 + 1).to_le_bytes());
        assert!(matches!(
            d.next_frame(0),
            Err(ProtocolError::FrameTooLarge(_))
        ));
    }

    #[test]
    fn rejects_digest_mismatch_and_length_mismatch() {
        // Hand-build a frame with tampered body but stale digest: rebuild the
        // raw JSON with mismatching digest.
        let body = serde_json::json!({"x":1});
        let frame = WireFrame {
            header: FrameHeader {
                payload_len: 7,
                payload_digest: Some(ArtifactDigest::of_bytes(b"whatever")),
                ..header("inspect")
            },
            body: body.clone(),
        };
        let json = serde_json::to_vec(&frame).unwrap();
        let mut raw = Vec::new();
        raw.extend_from_slice(&(json.len() as u32).to_le_bytes());
        raw.extend_from_slice(&json);
        let mut d = FrameDecoder::new();
        d.push(&raw);
        assert!(matches!(
            d.next_frame(0),
            Err(ProtocolError::LengthMismatch { .. }) | Err(ProtocolError::DigestMismatch)
        ));
    }

    #[test]
    fn rejects_unknown_fields() {
        let json = r#"{"header":{"protocol_version":1,"request_id":"r","operation":"x","sequence":0,"payload_len":2},"body":{},"extra":1}"#;
        let raw = json.as_bytes();
        let mut framed = Vec::new();
        framed.extend_from_slice(&(raw.len() as u32).to_le_bytes());
        framed.extend_from_slice(raw);
        let mut d = FrameDecoder::new();
        d.push(&framed);
        assert!(matches!(d.next_frame(0), Err(ProtocolError::Malformed(_))));
    }

    #[test]
    fn deadline_enforced() {
        let mut h = header("inspect");
        h.deadline_ms = Some(1000);
        let bytes = encode_frame(h, &serde_json::json!({}), false).unwrap();
        let mut d = FrameDecoder::new();
        d.push(&bytes);
        assert_eq!(d.next_frame(1000), Err(ProtocolError::DeadlineExpired));
        d.push(&[]);
        // fresh decoder for the valid-time case
        let mut d2 = FrameDecoder::new();
        d2.push(&bytes);
        assert!(d2.next_frame(999).unwrap().is_some());
    }

    #[test]
    fn duplicate_table_semantics() {
        let mut t = DuplicateTable::new();
        let d1 = RequestDigest::of_bytes(b"a");
        let d2 = RequestDigest::of_bytes(b"b");
        assert_eq!(t.check("r1", &d1), DuplicateOutcome::New);
        t.record("r1", d1.clone());
        assert_eq!(t.check("r1", &d1), DuplicateOutcome::Replay);
        assert_eq!(t.check("r1", &d2), DuplicateOutcome::Conflict);
    }

    #[test]
    fn sequence_guard_regressions() {
        let mut g = SequenceGuard::new();
        assert!(g.accept(0).is_ok());
        assert!(g.accept(5).is_ok());
        assert_eq!(
            g.accept(3),
            Err(ProtocolError::SequenceRegression {
                got: 3,
                expected: 6
            })
        );
    }
}
