//! The one clock/identity port every layer calls (ADR 0021, layer 1).
//!
//! Layers 1–4 read wall time and mint identifiers exclusively through
//! [`Runtime`]; direct `SystemTime`/`Uuid` calls outside this crate are the
//! drift the gate watches. Tests substitute a fake by implementing the two
//! required methods; [`Runtime::timestamp`], [`Runtime::id_with_prefix`] and
//! [`Runtime::random`] derive from them so a fake stays deterministic.

use crate::timestamp::ConversationTimestamp;

/// Host-supplied clock and identity source. Runtime code must not bypass it.
pub trait Runtime: Send + Sync {
    /// Epoch milliseconds.
    fn now_ms(&self) -> i64;
    /// A unique, opaque identifier (no prefix).
    fn id(&self) -> String;
    /// ISO-8601 UTC text with milliseconds, derived from [`Self::now_ms`] so
    /// a frozen clock freezes both views coherently. Covers every JavaScript
    /// `Date` millisecond (including the extended years chrono rejects) and
    /// fails fast on a host clock outside that range: durable rows keyed by
    /// this text must stay comparable with the TypeScript stores.
    fn timestamp(&self) -> String {
        ConversationTimestamp::from_epoch_millis(self.now_ms())
            .expect("host clock must fit JavaScript Date range")
            .wire_text()
            .expect("validated host clock")
            .into_owned()
    }
    /// A unique identifier carrying a record-shaped prefix (`"task_"`, …).
    fn id_with_prefix(&self, prefix: &str) -> String {
        format!("{prefix}{}", self.id())
    }
    /// A sample in `[0, 1)` for jitter. The default draws the identity port
    /// itself and mixes it through splitmix64, so every unique `id()` yields
    /// an independent sample and repeated calls within one millisecond still
    /// differ. A frozen fake with a constant id gives a constant sample —
    /// determinism where the test wants it, no process-global counter.
    fn random(&self) -> f64 {
        let mut z = fnv1a64(self.id().as_bytes());
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        // Take the top 53 bits: the largest integral double, so the result
        // lands uniformly on multiples of 2^-53 below 1.0.
        (z >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// 64-bit FNV-1a over bytes; the pre-mixer for [`Runtime::random`].
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xCB_F2_3D_3A_5E_64_4B_7Fu64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01B3);
    }
    hash
}

/// The production [`Runtime`]: system clock plus UUIDv7 identities.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemRuntime;

#[cfg(not(target_arch = "wasm32"))]
impl Runtime for SystemRuntime {
    fn now_ms(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .try_into()
            .unwrap_or(i64::MAX)
    }

    fn id(&self) -> String {
        uuid::Uuid::now_v7().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Frozen(i64);
    impl Runtime for Frozen {
        fn now_ms(&self) -> i64 {
            self.0
        }
        fn id(&self) -> String {
            "frozen".into()
        }
    }

    struct Counter(AtomicU64);
    impl Runtime for Counter {
        fn now_ms(&self) -> i64 {
            1_700_000_000_000
        }
        fn id(&self) -> String {
            format!("id-{}", self.0.fetch_add(1, Ordering::Relaxed))
        }
    }

    #[test]
    fn timestamp_covers_entire_javascript_date_range_without_chrono_panics() {
        for (epoch, expected) in [
            (-8_640_000_000_000_000, "-271821-04-20T00:00:00.000Z"),
            (8_640_000_000_000_000, "+275760-09-13T00:00:00.000Z"),
            (253_402_300_800_000, "+010000-01-01T00:00:00.000Z"),
            (-1, "1969-12-31T23:59:59.999Z"),
        ] {
            assert_eq!(Frozen(epoch).timestamp(), expected);
        }
    }

    #[test]
    #[should_panic(expected = "host clock must fit JavaScript Date range")]
    fn timestamp_rejects_clock_outside_explicit_precondition() {
        Frozen(8_640_000_000_000_001).timestamp();
    }

    #[test]
    fn random_is_in_unit_interval() {
        assert!((0.0..1.0).contains(&Frozen(1_700_000_000_000).random()));
    }

    #[test]
    fn random_advances_per_unique_id_within_one_millisecond() {
        let clock = Counter(AtomicU64::new(0));
        let first = clock.random();
        let second = clock.random();
        assert!((0.0..1.0).contains(&first) && (0.0..1.0).contains(&second));
        assert_ne!(first, second, "each fresh id must give a fresh sample");
    }

    #[test]
    fn random_of_a_frozen_id_is_deterministic() {
        let clock = Frozen(7);
        assert_eq!(clock.random(), clock.random());
    }

    #[test]
    fn id_with_prefix_carries_the_prefix() {
        assert!(Frozen(0).id_with_prefix("task_").starts_with("task_"));
    }
}
