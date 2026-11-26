use agentkit_sandbox_proto::guest::{self, Entry, FileData, OperationState, OutputData};
use agentkit_workspaces::{EntryKind, RelPath};

use super::*;

fn header(state: OperationState, next_seq: u64, len: u64, eof: bool) -> OutputData {
    OutputData {
        state,
        next_seq,
        truncated: false,
        eof,
        len,
    }
}

#[test]
fn output_sequences_must_cover_their_bytes() {
    let done = OperationState::Succeeded { exit: 0 };
    assert!(output(10, 100, &header(OperationState::Running, 15, 5, false)).is_ok());
    // A log the workload truncated: the advance passes the bytes read.
    assert!(output(10, 100, &header(OperationState::Running, 20, 5, false)).is_ok());
    assert!(output(10, 100, &header(done, 10, 0, true)).is_ok());
    let broken = [
        header(OperationState::Running, 9, 0, false),
        header(OperationState::Running, 14, 5, false),
        header(OperationState::Running, 111, 5, false),
        header(OperationState::Running, 15, 5, true),
        header(OperationState::Unknown, 15, 5, false),
    ];
    for bad in broken {
        assert!(output(10, 100, &bad).is_err(), "{bad:?}");
    }
}

fn file_header(size: u64, len: u64, binary: bool) -> FileData {
    FileData {
        size,
        mtime_ms: 0,
        binary,
        eof: true,
        len,
    }
}

#[test]
fn file_reads_stay_inside_the_file_and_the_request() {
    let asked = ReadAsked {
        offset: 6,
        max_file_bytes: 100,
        raw: false,
    };
    assert!(file(asked, &file_header(11, 5, false)).is_ok());
    assert!(
        file(asked, &file_header(11, 6, false)).is_err(),
        "past the end"
    );
    assert!(
        file(asked, &file_header(11, 1, true)).is_err(),
        "binary bytes"
    );
    assert!(
        file(asked, &file_header(101, 0, false)).is_err(),
        "too large"
    );
    let raw = ReadAsked { raw: true, ..asked };
    assert!(file(raw, &file_header(11, 5, true)).is_ok());
    let beyond = ReadAsked {
        offset: 50,
        ..asked
    };
    assert!(file(beyond, &file_header(11, 0, false)).is_ok());
}

fn entry(path: &str, kind: guest::EntryKind, size: Option<u64>) -> Entry {
    Entry {
        path: path.to_owned(),
        kind,
        size,
    }
}

#[test]
fn entries_lie_below_the_listing_within_its_depth() {
    let tree = RelPath::parse("tree").unwrap();
    let good = vec![
        entry("tree/a.txt", guest::EntryKind::File, Some(3)),
        entry("tree/d", guest::EntryKind::Directory, None),
        entry("tree/d/b.txt", guest::EntryKind::File, Some(3)),
    ];
    let listed = entries(&tree, 2, 10, good).unwrap();
    assert_eq!(listed.len(), 3);
    assert_eq!(listed[1].kind, EntryKind::Directory);
    let at_root = vec![entry("a.txt", guest::EntryKind::File, Some(1))];
    assert!(entries(&RelPath::root(), 1, 10, at_root).is_ok());
    let broken = [
        entry("../etc/passwd", guest::EntryKind::File, None),
        entry("/etc/passwd", guest::EntryKind::File, None),
        entry("tree/../other", guest::EntryKind::File, None),
        entry("tree//a.txt", guest::EntryKind::File, None),
        entry("other/a.txt", guest::EntryKind::File, None),
        entry("tree", guest::EntryKind::Directory, None),
        entry("treehouse/a", guest::EntryKind::File, None),
        entry("tree/d/e/f.txt", guest::EntryKind::File, None),
        entry("tree/d", guest::EntryKind::Directory, Some(4096)),
    ];
    for bad in broken {
        let shown = bad.path.clone();
        assert!(entries(&tree, 2, 10, vec![bad]).is_err(), "{shown}");
    }
    let many = vec![entry("tree/a", guest::EntryKind::File, None); 3];
    assert!(entries(&tree, 1, 2, many).is_err(), "over the count");
}
