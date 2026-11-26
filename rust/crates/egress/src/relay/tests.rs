use std::sync::atomic::{AtomicU64, Ordering};

use super::{Bounds, Budget, Flow, pump};

#[test]
fn budget_never_grants_past_its_limit() {
    let budget = Budget::new(10);
    assert_eq!(budget.take(4), 4);
    assert!(!budget.spent());
    assert_eq!(budget.take(8), 6);
    assert!(budget.spent());
    assert_eq!(budget.take(1), 0);
}

#[tokio::test]
async fn pump_forwards_early_bytes_then_stops_at_the_bound() {
    let budget = Budget::new(5);
    let moved = AtomicU64::new(0);
    let mut out = Vec::new();
    let source: &[u8] = b"cdefgh";
    let bounds = Bounds {
        budget: &budget,
        limit: None,
    };
    let flow = pump(source, &mut out, b"ab".to_vec(), bounds, &moved).await;
    assert_eq!(flow, Flow::Spent);
    assert_eq!(out, b"abcde");
    assert_eq!(moved.load(Ordering::Relaxed), 5);
}

#[tokio::test]
async fn pump_reports_eof_within_the_bound() {
    let budget = Budget::new(100);
    let moved = AtomicU64::new(0);
    let mut out = Vec::new();
    let source: &[u8] = b"hello";
    let bounds = Bounds {
        budget: &budget,
        limit: None,
    };
    let flow = pump(source, &mut out, Vec::new(), bounds, &moved).await;
    assert_eq!(flow, Flow::Eof);
    assert_eq!(out, b"hello");
}

async fn limited(source: &[u8], early: &[u8], limit: u64) -> (Flow, Vec<u8>, u64) {
    let budget = Budget::new(100);
    let moved = AtomicU64::new(0);
    let mut out = Vec::new();
    let bounds = Bounds {
        budget: &budget,
        limit: Some(limit),
    };
    let flow = pump(source, &mut out, early.to_vec(), bounds, &moved).await;
    (flow, out, moved.load(Ordering::Relaxed))
}

#[tokio::test]
async fn a_limited_pump_stops_reading_at_its_limit() {
    // The limit ends inside the early bytes: the rest of them and the
    // source are never forwarded.
    let (flow, out, moved) = limited(b"never", b"bodyNEXT", 4).await;
    assert_eq!((flow, out.as_slice(), moved), (Flow::Done, &b"body"[..], 4));
    // The limit ends inside a read.
    let (flow, out, _) = limited(b"dyNEXT REQUEST", b"bo", 4).await;
    assert_eq!((flow, out.as_slice()), (Flow::Done, &b"body"[..]));
    // No body: nothing is read at all.
    let (flow, out, _) = limited(b"GET /again", b"", 0).await;
    assert_eq!((flow, out.as_slice()), (Flow::Done, &b""[..]));
    // The source ends first: an ordinary end.
    let (flow, out, _) = limited(b"bo", b"", 4).await;
    assert_eq!((flow, out.as_slice()), (Flow::Eof, &b"bo"[..]));
}
