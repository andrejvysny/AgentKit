use super::{DenialLog, DenialReason, DenialRecord};

fn record(host: &str) -> DenialRecord {
    DenialRecord {
        seq: 0,
        host: host.to_owned(),
        port: 443,
        at_ms: 7,
        reason: DenialReason::NotGranted,
        operation_id: None,
    }
}

fn seqs(log: &DenialLog, cursor: u64) -> (Vec<u64>, u64, u64) {
    let batch = log.after(cursor);
    let seqs = batch.denials.iter().map(|d| d.seq).collect();
    (seqs, batch.cursor, batch.missed)
}

#[test]
fn empty_log_answers_cursor_zero() {
    let log = DenialLog::new(4);
    assert_eq!(seqs(&log, 0), (vec![], 0, 0));
    // A cursor from elsewhere is clamped to what this log has.
    assert_eq!(seqs(&log, 9), (vec![], 0, 0));
}

#[test]
fn cursor_reads_after_and_rereads_are_stable() {
    let mut log = DenialLog::new(4);
    for host in ["a", "b", "c"] {
        log.push(record(host));
    }
    assert_eq!(seqs(&log, 0), (vec![1, 2, 3], 3, 0));
    assert_eq!(seqs(&log, 2), (vec![3], 3, 0));
    assert_eq!(seqs(&log, 3), (vec![], 3, 0));
    assert_eq!(
        seqs(&log, 0),
        (vec![1, 2, 3], 3, 0),
        "reading removes nothing"
    );
    assert_eq!(log.total(), 3);
}

#[test]
fn overflow_drops_oldest_and_counts_the_gap() {
    let mut log = DenialLog::new(3);
    for host in ["a", "b", "c", "d", "e"] {
        log.push(record(host));
    }
    let batch = log.after(0);
    let hosts: Vec<&str> = batch.denials.iter().map(|d| d.host.as_str()).collect();
    assert_eq!(hosts, ["c", "d", "e"]);
    assert_eq!((batch.cursor, batch.missed), (5, 2));
    assert_eq!(seqs(&log, 1), (vec![3, 4, 5], 5, 1));
    assert_eq!(seqs(&log, 2), (vec![3, 4, 5], 5, 0));
    assert_eq!(seqs(&log, 4), (vec![5], 5, 0));
    assert_eq!(seqs(&log, 5), (vec![], 5, 0));
    log.push(record("f"));
    assert_eq!(seqs(&log, 5), (vec![6], 6, 0));
}
