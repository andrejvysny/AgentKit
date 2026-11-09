//! Foundation layer (ADR 0021, layer 1): the single sha-256 hex and
//! canonical-JSON implementations every other crate calls.
//!
//! No async runtime and no filesystem access, so the crate also builds for
//! `wasm32-unknown-unknown`.

mod canonical;
mod hash;

pub use canonical::{canonical_json, canonical_sha256};
pub use hash::sha256_hex;
