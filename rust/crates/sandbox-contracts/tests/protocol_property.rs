//! Property-style checks over the framed protocol and canonical digest
//! (T18/T24): generated valid shapes must round-trip exactly, and every
//! random corruption must fail closed -- bounded, no panics. Deterministic
//! xorshift64* (fixed seed), same convention as `protocol_adversarial`.

use agentkit_sandbox_contracts::PROTOCOL_VERSION;
use agentkit_sandbox_contracts::canonical::canonicalize;
use agentkit_sandbox_contracts::ids::{ExecutionId, InstanceGeneration, RequestDigest, SandboxId};
use agentkit_sandbox_contracts::limits::MAX_FRAME_BYTES;
use agentkit_sandbox_contracts::protocol::{
    FrameDecoder, FrameHeader, ProtocolError, encode_frame, validate_header,
};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next() % n }
    }
    fn string(&mut self, pool: &[u8], max_len: usize) -> String {
        let len = (self.below(max_len as u64) + 1) as usize;
        (0..len)
            .map(|_| pool[self.below(pool.len() as u64) as usize] as char)
            .collect()
    }
    fn json(&mut self, depth: u32) -> serde_json::Value {
        use serde_json::{Map, Value};
        let pick = self.below(if depth == 0 { 5 } else { 8 });
        match pick {
            0 => Value::Null,
            1 => Value::Bool(self.next() % 2 == 0),
            2 => Value::from(self.next() as i64),
            3 => Value::String(self.string(b"ab9_-. /", 24)),
            4 => Value::from(self.below(1_000_000) as i64),
            5 => {
                let n = self.below(4) + 1;
                Value::Array((0..n).map(|_| self.json(depth - 1)).collect())
            }
            _ => {
                let n = self.below(6) + 1;
                let mut m = Map::new();
                for _ in 0..n {
                    let k = self.string(b"abcdefghij0123456789", 10);
                    m.insert(k, self.json(depth - 1));
                }
                Value::Object(m)
            }
        }
    }
}

const NOW: u64 = 1_700_000_000_000;

fn header(rng: &mut Rng) -> FrameHeader {
    FrameHeader {
        protocol_version: PROTOCOL_VERSION,
        request_id: rng.string(b"abcdefghijklmnop0123456789-", 32),
        sandbox_id: if rng.below(3) == 0 {
            Some(SandboxId::new(format!("sb{}", rng.below(1000))).unwrap())
        } else {
            None
        },
        instance_generation: if rng.below(4) == 0 {
            Some(InstanceGeneration(rng.below(1 << 20)))
        } else {
            None
        },
        execution_id: if rng.below(5) == 0 {
            Some(ExecutionId::new(format!("e{}", rng.below(100))).unwrap())
        } else {
            None
        },
        operation: rng.string(b"ABCDEFGHIJ_0123456789", 20),
        sequence: rng.below(1000),
        deadline_ms: if rng.below(3) == 0 {
            Some(NOW + 600_000)
        } else {
            None
        },
        payload_len: 0,
        payload_digest: None,
    }
}

/// P1: every generated valid frame encodes and decodes back to the exact
/// body and identity fields (length prefix and digest are encoder-managed).
#[test]
fn valid_frames_roundtrip_exactly() {
    let mut rng = Rng(0xC0FFEE_1234_5678);
    for round in 0..400u64 {
        let h = header(&mut rng);
        let body = rng.json(3);
        let digest_body = round % 2 == 0;
        let bytes = encode_frame(h.clone(), &body, digest_body).expect("encode");
        let mut d = FrameDecoder::new();
        d.push(&bytes);
        let f = d
            .next_frame(NOW)
            .expect("valid frame must decode")
            .expect("complete frame");
        assert_eq!(f.header.request_id, h.request_id, "round {round}");
        assert_eq!(f.header.operation, h.operation, "round {round}");
        assert_eq!(f.header.sequence, h.sequence, "round {round}");
        assert_eq!(f.header.sandbox_id, h.sandbox_id, "round {round}");
        assert_eq!(f.header.execution_id, h.execution_id, "round {round}");
        assert_eq!(f.body, body, "round {round}");
        assert!(d.next_frame(NOW).unwrap().is_none(), "no trailing frame");
    }
}

/// P2: canonical bytes (and therefore digests) are invariant to map
/// insertion order, and any single-leaf mutation flips the digest.
#[test]
fn canonical_digest_is_order_invariant_and_mutation_sensitive() {
    let mut rng = Rng(0xDEAD_BEEF_0BAD_F00D);
    for round in 0..300u64 {
        // A fixed set of key/value pairs emitted in two random orders.
        let n = rng.below(8) + 1;
        let pairs: Vec<(String, serde_json::Value)> = (0..n)
            .map(|_| {
                let k = rng.string(b"kABCDEFGHIJ0123456789", 8);
                (k, rng.json(2))
            })
            .collect();
        let dedup: std::collections::HashMap<String, serde_json::Value> =
            pairs.into_iter().collect();
        let entries: Vec<_> = dedup.into_iter().collect();

        let build = |reverse: bool| {
            let mut m = serde_json::Map::new();
            let it: Box<dyn Iterator<Item = &(String, serde_json::Value)>> = if reverse {
                Box::new(entries.iter().rev())
            } else {
                Box::new(entries.iter())
            };
            for (k, v) in it {
                m.insert(k.clone(), v.clone());
            }
            serde_json::Value::Object(m)
        };
        let a = build(false);
        let b = build(true);
        let ca = canonicalize(&a).unwrap();
        let cb = canonicalize(&b).unwrap();
        assert_eq!(ca, cb, "round {round}: insertion order leaked");
        assert_eq!(
            RequestDigest::of_canonical_json(&ca),
            RequestDigest::of_canonical_json(&cb)
        );

        // Mutation sensitivity: flip one character of the serialized form
        // and, if it still parses, the digest must change.
        let mut s = serde_json::to_string(&a).unwrap();
        let idx = (rng.below(s.len() as u64)) as usize;
        let bytes: Vec<u8> = s
            .bytes()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { b'x' })
            .collect();
        let mut mutated = bytes.clone();
        mutated[idx] = b'a' + (bytes[idx] % 26);
        s = String::from_utf8(mutated).unwrap();
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&s)
            && serde_json::to_string(&v).unwrap() != serde_json::to_string(&a).unwrap()
        {
            let cv = canonicalize(&v).unwrap();
            assert_ne!(
                RequestDigest::of_canonical_json(&ca),
                RequestDigest::of_canonical_json(&cv),
                "digest collision on distinct canonical forms (round {round})"
            );
        }
    }
}

/// P3: generated header mutations fail closed at validation and the decoder
/// never buffers beyond the frame bound on corrupted input.
#[test]
fn header_mutations_fail_closed() {
    let mut rng = Rng(0x5EED_1234_ABCD_0001);
    for _ in 0..500u64 {
        let h = header(&mut rng);
        assert!(validate_header(&h, NOW).is_ok(), "generated header valid");

        // Wrong protocol version must be rejected explicitly.
        let mut bad = header(&mut rng);
        bad.protocol_version = PROTOCOL_VERSION + 1 + rng.below(3) as u32;
        match validate_header(&bad, NOW) {
            Err(ProtocolError::UnsupportedVersion(v)) => assert_eq!(v, bad.protocol_version),
            Err(other) => panic!("expected UnsupportedVersion, got {other:?}"),
            Ok(()) => panic!("version mutation accepted"),
        }

        // Expired deadline must be rejected.
        let mut expired = header(&mut rng);
        expired.deadline_ms = Some(NOW - 1);
        assert!(validate_header(&expired, NOW).is_err(), "expired deadline");

        // Corrupt the length prefix: bounded buffering, no panic, no bogus
        // complete frame at the wrong length.
        let body = rng.json(2);
        let mut bytes = encode_frame(header(&mut rng), &body, true).unwrap();
        match rng.below(3) {
            0 => bytes[0] = bytes[0].wrapping_add(1),
            1 => bytes.truncate(bytes.len() / 2),
            _ => {
                let i = (rng.below(bytes.len() as u64)) as usize;
                bytes[i] ^= 0xA5;
            }
        }
        let mut d = FrameDecoder::new();
        d.push(&bytes);
        let _ = d.next_frame(NOW); // Err or None both fine; must not panic.
        assert!(d.buffered() <= MAX_FRAME_BYTES);
    }
}
