//! Deterministic canonical JSON for digests and idempotency.
//!
//! Digests (`RequestDigest`, `PolicyDigest`, …) are computed over this
//! canonical form so that semantically identical payloads always produce the
//! same digest and conflicting payloads never collide into a false
//! "duplicate". Objects are emitted with keys sorted by raw byte order, no
//! insignificant whitespace, and floats are rejected outright (digest inputs
//! must be integral or textual).

use serde_json::{Map, Number, Value};

/// Returns the canonical byte encoding of `value`, or an error if it contains
/// a float (which has no stable canonical text form here).
pub fn canonicalize(value: &Value) -> Result<Vec<u8>, CanonicalError> {
    let mut out = Vec::new();
    write_value(value, &mut out)?;
    Ok(out)
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum CanonicalError {
    #[error("floats are not permitted in canonical digest inputs: {0}")]
    Float(f64),
    #[error("value is not representable as canonical JSON: {0}")]
    Encode(String),
}

fn write_value(value: &Value, out: &mut Vec<u8>) -> Result<(), CanonicalError> {
    match value {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(true) => out.extend_from_slice(b"true"),
        Value::Bool(false) => out.extend_from_slice(b"false"),
        Value::Number(n) => write_number(n, out)?,
        Value::String(s) => write_string(s, out),
        Value::Array(items) => {
            out.push(b'[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_value(item, out)?;
            }
            out.push(b']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push(b'{');
            for (i, key) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_string(key, out);
                out.push(b':');
                write_value(&map[*key], out)?;
            }
            out.push(b'}');
        }
    }
    Ok(())
}

fn write_number(n: &Number, out: &mut Vec<u8>) -> Result<(), CanonicalError> {
    if let Some(i) = n.as_i64() {
        out.extend_from_slice(i.to_string().as_bytes());
    } else if let Some(u) = n.as_u64() {
        out.extend_from_slice(u.to_string().as_bytes());
    } else if let Some(f) = n.as_f64() {
        // With `arbitrary_precision` a value only reaches here if it parsed
        // as a float token. Integral-valued floats are emitted as integers;
        // genuinely fractional values are rejected as non-canonical.
        if f.fract() == 0.0 && f.abs() < 9.0e18 {
            out.extend_from_slice((f as i64).to_string().as_bytes());
        } else {
            return Err(CanonicalError::Float(f));
        }
    } else {
        // arbitrary_precision keeps the literal text; huge integers are
        // emitted verbatim (canonical as parsed).
        out.extend_from_slice(n.to_string().as_bytes());
    }
    Ok(())
}

fn write_string(s: &str, out: &mut Vec<u8>) {
    // Compact JSON string escaping, byte-for-byte deterministic.
    out.push(b'"');
    for c in s.chars() {
        match c {
            '"' => out.extend_from_slice(b"\\\""),
            '\\' => out.extend_from_slice(b"\\\\"),
            '\n' => out.extend_from_slice(b"\\n"),
            '\r' => out.extend_from_slice(b"\\r"),
            '\t' => out.extend_from_slice(b"\\t"),
            '\u{08}' => out.extend_from_slice(b"\\b"),
            '\u{0c}' => out.extend_from_slice(b"\\f"),
            c if (c as u32) < 0x20 => {
                out.extend_from_slice(format!("\\u{:04x}", c as u32).as_bytes());
            }
            c => {
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
        }
    }
    out.push(b'"');
}

/// Sort map keys deterministically (used by tests and callers building
/// payloads).
/// THE canonical `request_digest` convention (single authority for host and
/// worker): serialize, recursively normalize every `request_digest` string
/// field to `""`, canonicalize, SHA-256. Bindings that embed a digest field
/// MUST use this function — never a private copy.
pub fn request_digest_of<T: serde::Serialize>(
    request: &T,
) -> Result<crate::ids::RequestDigest, CanonicalError> {
    let mut value =
        serde_json::to_value(request).map_err(|e| CanonicalError::Encode(e.to_string()))?;
    normalize_digest_field(&mut value);
    let bytes = canonicalize(&value)?;
    Ok(crate::ids::RequestDigest::of_canonical_json(&bytes))
}

/// Canonical self-digest for stage manifests (§11): serialize, zero every
/// `manifest_digest` string field, canonicalize, SHA-256. A manifest's
/// `manifest_digest` MUST be produced by this function — the value is not
/// part of its own digest (same convention as `request_digest`). Host,
/// worker, guest, and simulator all call THIS function.
pub fn manifest_digest_of<T: serde::Serialize>(
    manifest: &T,
) -> Result<crate::ids::ArtifactDigest, CanonicalError> {
    let mut value =
        serde_json::to_value(manifest).map_err(|e| CanonicalError::Encode(e.to_string()))?;
    normalize_manifest_digest_field(&mut value);
    let bytes = canonicalize(&value)?;
    Ok(crate::ids::ArtifactDigest::of_canonical_json(&bytes))
}

fn normalize_manifest_digest_field(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (k, v) in map.iter_mut() {
                if k == "manifest_digest" {
                    *v = Value::String(String::new());
                } else {
                    normalize_manifest_digest_field(v);
                }
            }
        }
        Value::Array(items) => {
            for v in items {
                normalize_manifest_digest_field(v);
            }
        }
        _ => {}
    }
}

fn normalize_digest_field(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (k, v) in map.iter_mut() {
                if k == "request_digest" {
                    *v = Value::String(String::new());
                } else {
                    normalize_digest_field(v);
                }
            }
        }
        Value::Array(items) => {
            for v in items {
                normalize_digest_field(v);
            }
        }
        _ => {}
    }
}

pub fn sorted_map(pairs: Vec<(&'static str, Value)>) -> Map<String, Value> {
    let mut map = Map::new();
    for (k, v) in pairs {
        map.insert(k.to_string(), v);
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(text: &str) -> Value {
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn key_order_irrelevant() {
        let a = v(r#"{"b":1,"a":{"d":2,"c":[3,4]}}"#);
        let b = v(r#"{"a":{"c":[3,4],"d":2},"b":1}"#);
        assert_eq!(canonicalize(&a).unwrap(), canonicalize(&b).unwrap());
    }

    #[test]
    fn semantic_difference_changes_digest_bytes() {
        let a = v(r#"{"x":[1,2]}"#);
        let b = v(r#"{"x":[2,1]}"#);
        assert_ne!(canonicalize(&a).unwrap(), canonicalize(&b).unwrap());
    }

    #[test]
    fn integral_floats_normalized_int_rejected_float_kept() {
        // "1.0" is integral: emits same bytes as 1 (so 1 and 1.0 digest
        // identically; they are the same JSON number semantically).
        let a = v(r#"{"n":1}"#);
        let b = v(r#"{"n":1.0}"#);
        assert_eq!(canonicalize(&a).unwrap(), canonicalize(&b).unwrap());
        assert!(canonicalize(&v(r#"{"n":1.5}"#)).is_err());
    }

    #[test]
    fn control_chars_escaped_deterministically() {
        let a = v("\"\\u0001x\"");
        assert_eq!(canonicalize(&a).unwrap(), b"\"\\u0001x\"");
    }
}

#[cfg(test)]
mod request_digest_tests {
    use super::*;
    use crate::ids::RequestDigest;
    use serde::Serialize;

    #[derive(Serialize)]
    struct Body {
        command: String,
        request_digest: String,
        nested: Inner,
    }
    #[derive(Serialize)]
    struct Inner {
        request_digest: String,
        v: u64,
    }

    #[test]
    fn digest_ignores_declared_and_nested_digest_fields() {
        // A binding that FORGETS to clear its digest fields still digests
        // identically to the cleared form: the normalization is the authority.
        let forged = Body {
            command: "make".into(),
            request_digest: "a".repeat(64),
            nested: Inner {
                request_digest: "b".repeat(64),
                v: 7,
            },
        };
        let cleared = Body {
            command: "make".into(),
            request_digest: String::new(),
            nested: Inner {
                request_digest: String::new(),
                v: 7,
            },
        };
        assert_eq!(
            request_digest_of(&forged).unwrap(),
            request_digest_of(&cleared).unwrap()
        );
        // And equals the manual convention.
        let mut v = serde_json::to_value(&cleared).unwrap();
        normalize_digest_field(&mut v);
        assert_eq!(
            request_digest_of(&forged).unwrap(),
            RequestDigest::of_canonical_json(&canonicalize(&v).unwrap())
        );
    }

    #[derive(Serialize)]
    struct OptBody {
        command: String,
        request_digest: Option<String>,
    }

    #[test]
    fn null_and_empty_digest_fields_unify() {
        // serde renders None as null; the cleared binding renders "". Both
        // MUST digest identically or host/worker would disagree.
        let none = OptBody {
            command: "x".into(),
            request_digest: None,
        };
        let empty = OptBody {
            command: "x".into(),
            request_digest: Some(String::new()),
        };
        assert_eq!(
            request_digest_of(&none).unwrap(),
            request_digest_of(&empty).unwrap()
        );
    }
}
