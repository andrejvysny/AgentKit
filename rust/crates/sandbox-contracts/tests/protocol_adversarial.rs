//! Adversarial protocol framing tests (T18, T24 L1 subset).
//!
//! Feeds malformed, oversized, replayed, and tampered frames through the
//! decoder and duplicate/sequence guards: every hostile shape must fail
//! closed without panics or unbounded buffering.

use agentkit_sandbox_contracts::error::SandboxErrorCode;
use agentkit_sandbox_contracts::ids::{InstanceGeneration, SandboxId};
use agentkit_sandbox_contracts::protocol::{
    DuplicateOutcome, DuplicateTable, FrameDecoder, FrameHeader, ProtocolError, SequenceGuard,
    encode_frame, validate_header,
};
use agentkit_sandbox_contracts::{PROTOCOL_VERSION, ids::RequestDigest};

fn header(op: &str) -> FrameHeader {
    FrameHeader {
        protocol_version: PROTOCOL_VERSION,
        request_id: "r-1".into(),
        sandbox_id: Some(SandboxId::new("sb1").unwrap()),
        instance_generation: Some(InstanceGeneration(1)),
        execution_id: None,
        operation: op.into(),
        sequence: 0,
        deadline_ms: None,
        payload_len: 0,
        payload_digest: None,
    }
}

fn frame_bytes(json: &str) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(json.len() as u32).to_le_bytes());
    out.extend_from_slice(json.as_bytes());
    out
}

#[test]
fn garbage_input_never_panics_and_bounded() {
    // Deterministic pseudo-random fuzz (xorshift32, seed fixed like the
    // repo conformance vectors). Any panics or oversized buffering here are
    // failures of the decoder contract.
    let mut state: u32 = 0x1234_5678;
    let mut rnd = move || {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        state
    };
    for round in 0..2000 {
        let len = (rnd() % 256) as usize;
        let mut buf = Vec::with_capacity(len);
        for _ in 0..(len / 4) + 1 {
            buf.extend_from_slice(&rnd().to_le_bytes());
        }
        buf.truncate(len);
        let mut d = FrameDecoder::new();
        d.push(&buf);
        let _ = d.next_frame(round as u64); // swallow Err; must not panic
        assert!(
            d.buffered() <= agentkit_sandbox_contracts::limits::MAX_FRAME_BYTES,
            "decoder buffered beyond bound"
        );
    }
}

#[test]
fn truncated_and_stacked_frames() {
    let bytes = encode_frame(header("inspect"), &serde_json::json!({"a": 1}), false).unwrap();
    let mut d = FrameDecoder::new();
    // Half a frame: no output, no error.
    d.push(&bytes[..bytes.len() / 2]);
    assert!(d.next_frame(0).unwrap().is_none());
    // Complete: exactly one frame.
    d.push(&bytes[bytes.len() / 2..]);
    assert!(d.next_frame(0).unwrap().is_some());
    assert!(d.next_frame(0).unwrap().is_none());
    // Three frames at once pop in order.
    let mut d = FrameDecoder::new();
    for i in 0..3 {
        let mut h = header("inspect");
        h.request_id = format!("r{i}");
        d.push(&encode_frame(h, &serde_json::json!({"i": i}), false).unwrap());
    }
    let mut ids = Vec::new();
    while let Some(f) = d.next_frame(0).unwrap() {
        ids.push(f.header.request_id);
    }
    assert_eq!(ids, vec!["r0", "r1", "r2"]);
}

#[test]
fn unknown_fields_rejected_at_every_level() {
    // extra top-level field
    assert!(matches!(
        decode(
            r#"{"header":{"protocol_version":1,"request_id":"r","operation":"x","sequence":0,"payload_len":2},"body":{},"x":1}"#
        ),
        Err(ProtocolError::Malformed(_))
    ));
    // extra header field
    assert!(matches!(
        decode(
            r#"{"header":{"protocol_version":1,"request_id":"r","operation":"x","sequence":0,"payload_len":2,"evil":true},"body":{}}"#
        ),
        Err(ProtocolError::Malformed(_))
    ));
    // missing header field
    assert!(matches!(
        decode(
            r#"{"header":{"protocol_version":1,"operation":"x","sequence":0,"payload_len":2},"body":{}}"#
        ),
        Err(ProtocolError::Malformed(_))
    ));
}

fn decode(
    json: &str,
) -> Result<Option<agentkit_sandbox_contracts::protocol::WireFrame>, ProtocolError> {
    let mut d = FrameDecoder::new();
    d.push(&frame_bytes(json));
    d.next_frame(0)
}

#[test]
fn version_confusion_rejected() {
    for bad in [0, 2, u32::MAX] {
        let mut h = header("inspect");
        h.protocol_version = bad;
        let bytes = encode_frame(h, &serde_json::json!({}), false).unwrap();
        let mut d = FrameDecoder::new();
        d.push(&bytes);
        assert_eq!(d.next_frame(0), Err(ProtocolError::UnsupportedVersion(bad)));
    }
}

#[test]
fn stale_generation_and_deadlines_rejected() {
    // instance_generation 0 cannot even deserialize; explicit validate:
    let mut h = header("inspect");
    h.instance_generation = Some(InstanceGeneration(0));
    assert_eq!(
        validate_header(&h, 0),
        Err(ProtocolError::InvalidHeader("instance_generation"))
    );
    // deadline in the past
    let mut h = header("inspect");
    h.deadline_ms = Some(5);
    assert_eq!(validate_header(&h, 6), Err(ProtocolError::DeadlineExpired));
    assert!(validate_header(&h, 4).is_ok());
}

#[test]
fn duplicate_suppression_matrix() {
    let mut t = DuplicateTable::new();
    let a = RequestDigest::of_bytes(b"a");
    let b = RequestDigest::of_bytes(b"b");
    assert_eq!(t.check("r", &a), DuplicateOutcome::New);
    t.record("r", a.clone());
    assert_eq!(t.check("r", &a), DuplicateOutcome::Replay);
    assert_eq!(t.check("r", &b), DuplicateOutcome::Conflict);
    // Eviction bound: after flooding, the oldest entries are gone — replay
    // of an evicted id reads as New (safety then rests on deadline and
    // generation checks, per the table's contract).
    for i in 0..(agentkit_sandbox_contracts::limits::MAX_DUPLICATE_TABLE + 100) {
        t.record(format!("x{i}"), RequestDigest::of_bytes(b"c"));
    }
    assert_eq!(
        t.check("x1", &RequestDigest::of_bytes(b"c")),
        DuplicateOutcome::New,
        "oldest entries must be evicted (bounded memory)"
    );
    assert_eq!(
        t.check("r", &a),
        DuplicateOutcome::New,
        "the first entry was evicted too"
    );
    // A recently recorded entry still suppresses.
    assert_eq!(
        t.check("x9999", &RequestDigest::of_bytes(b"c")),
        DuplicateOutcome::New
    );
    t.record("x9999", RequestDigest::of_bytes(b"c"));
    assert_eq!(
        t.check("x9999", &RequestDigest::of_bytes(b"c")),
        DuplicateOutcome::Replay
    );
}

#[test]
fn sequence_guard_allows_gaps_rejects_regressions() {
    let mut g = SequenceGuard::new();
    for s in [0, 1, 7, 8, 100] {
        assert!(g.accept(s).is_ok(), "gap/monotonic must pass: {s}");
    }
    assert!(g.accept(99).is_err());
    assert!(g.accept(100).is_err(), "equal-to-last is a regression");
    assert!(g.accept(101).is_ok());
}

#[test]
fn oversized_frame_rejected_before_body_arrives() {
    let mut d = FrameDecoder::new();
    d.push(&(agentkit_sandbox_contracts::limits::MAX_FRAME_BYTES as u32 + 1).to_le_bytes());
    assert!(matches!(
        d.next_frame(0),
        Err(ProtocolError::FrameTooLarge(_))
    ));
    // The encoder itself refuses to build an oversized frame.
    let big = serde_json::json!({"blob": "A".repeat(agentkit_sandbox_contracts::limits::MAX_FRAME_BYTES)});
    assert!(matches!(
        encode_frame(header("fs_write"), &big, false),
        Err(ProtocolError::FrameTooLarge(_))
    ));
}

#[test]
fn digest_binding_tamper_detection() {
    let bytes = encode_frame(
        header("fs_write"),
        &serde_json::json!({"body_b64": "AAAA"}),
        true,
    )
    .unwrap();
    // Flip one payload byte inside the body (base64 char), keep framing.
    let mut tampered = bytes.clone();
    let last = tampered.len() - 6; // near "AAAA"
    tampered[last] ^= 0x20;
    // If the flip landed in structural JSON (a quote etc.), json parse
    // fails — both are acceptable refusals. Accept no Ok.
    let mut d = FrameDecoder::new();
    d.push(&tampered);
    assert!(!matches!(d.next_frame(0), Ok(Some(_))));
}

#[test]
fn error_mapping_is_protocol_incompatible() {
    let err: agentkit_sandbox_contracts::error::SandboxError = ProtocolError::DigestMismatch.into();
    assert_eq!(err.code, SandboxErrorCode::ProtocolIncompatible);
}
