//! Files: atomic, digest-checked writes, bounded reads and listings, reads
//! beside a running command, and a volume that outlives generations.

use std::collections::BTreeSet;
use std::sync::Arc;

use agentkit_workspaces::{
    EntryKind, Generation, ListEntries, ListEntry, OperationState, PathRoot, ReadFile,
    SandboxError, StopProof,
};

use super::commands::SLEEP;
use super::{
    SandboxHarness, assert_not_found, dispatch_new, exec, list_req, port, read_all, read_req, rel,
    run_op, start, write, write_claiming, write_home,
};

pub(super) async fn write_then_read_round_trips(h: Arc<SandboxHarness>) {
    let m = start(&h, 1).await;
    let done = run_op(&h, &m, &write("op-1", &m, "dir/a.txt", b"hello world")).await;
    assert_eq!(done.state, OperationState::Succeeded { exit: 0 });
    let whole = port(h.backend.read_file(&m, &read_req("dir/a.txt")).await);
    assert_eq!(whole.bytes, b"hello world");
    assert_eq!((whole.size, whole.binary, whole.eof), (11, false, true));
    let part = ReadFile {
        offset: 6,
        limit_bytes: 3,
        ..read_req("dir/a.txt")
    };
    let slice = port(h.backend.read_file(&m, &part).await);
    assert_eq!(
        (slice.bytes.as_slice(), slice.size, slice.eof),
        (&b"wor"[..], 11, false)
    );
    run_op(&h, &m, &write("op-2", &m, "dir/a.txt", b"bye")).await;
    assert_eq!(
        read_all(&h, &m, "dir/a.txt").await,
        b"bye",
        "a write replaces"
    );
}

/// A write whose bytes do not match its digest never succeeds and leaves
/// the target as it was.
pub(super) async fn write_with_wrong_digest_changes_nothing(h: Arc<SandboxHarness>) {
    let m = start(&h, 1).await;
    run_op(&h, &m, &write("op-1", &m, "a.txt", b"v1")).await;
    let bad = write_claiming("op-2", &m, "a.txt", b"v2", b"something else");
    let done = run_op(&h, &m, &bad).await;
    assert!(
        !matches!(done.state, OperationState::Succeeded { .. }),
        "{:?}",
        done.state
    );
    assert_eq!(read_all(&h, &m, "a.txt").await, b"v1");
    let fresh = write_claiming("op-3", &m, "b.txt", b"v2", b"something else");
    run_op(&h, &m, &fresh).await;
    assert_not_found(h.backend.read_file(&m, &read_req("b.txt")).await);
}

pub(super) async fn read_refuses_missing_directories_and_large_files(h: Arc<SandboxHarness>) {
    let m = start(&h, 1).await;
    run_op(&h, &m, &write("op-1", &m, "dir/a.txt", b"hello world")).await;
    assert_not_found(h.backend.read_file(&m, &read_req("missing.txt")).await);
    let dir = h.backend.read_file(&m, &read_req("dir")).await;
    assert!(
        matches!(dir, Err(SandboxError::NotARegularFile { .. })),
        "{dir:?}"
    );
    let capped = ReadFile {
        max_file_bytes: 4,
        ..read_req("dir/a.txt")
    };
    let large = h.backend.read_file(&m, &capped).await;
    assert_eq!(
        large,
        Err(SandboxError::FileTooLarge { size: 11, limit: 4 })
    );
}

fn entry(path: &str, kind: EntryKind, size: Option<u64>) -> ListEntry {
    ListEntry {
        path: rel(path),
        kind,
        size,
    }
}

fn as_set(entries: Vec<ListEntry>) -> BTreeSet<String> {
    entries
        .into_iter()
        .map(|entry| format!("{entry:?}"))
        .collect()
}

pub(super) async fn list_honours_depth_and_limit(h: Arc<SandboxHarness>) {
    let m = start(&h, 1).await;
    for (n, path) in ["tree/a.txt", "tree/d/b.txt", "tree/d/e/c.txt"]
        .iter()
        .enumerate()
    {
        run_op(&h, &m, &write(&format!("op-{n}"), &m, path, b"abc")).await;
    }
    let a = entry("tree/a.txt", EntryKind::File, Some(3));
    let d = entry("tree/d", EntryKind::Directory, None);
    let b = entry("tree/d/b.txt", EntryKind::File, Some(3));
    let e = entry("tree/d/e", EntryKind::Directory, None);
    let c = entry("tree/d/e/c.txt", EntryKind::File, Some(3));
    let cases = [
        (1, vec![a.clone(), d.clone()]),
        (2, vec![a.clone(), d.clone(), b.clone(), e.clone()]),
        (4, vec![a, d, b, e, c]),
    ];
    for (depth, expected) in cases {
        let listing = port(h.backend.list(&m, &list_req("tree", depth, 500)).await);
        assert!(!listing.truncated, "depth {depth}");
        assert_eq!(as_set(listing.entries), as_set(expected), "depth {depth}");
    }
    let capped = port(h.backend.list(&m, &list_req("tree", 4, 2)).await);
    assert_eq!((capped.entries.len(), capped.truncated), (2, true));
    assert_not_found(h.backend.list(&m, &list_req("absent", 1, 10)).await);
}

/// Reads take no lease: they run while a command runs.
pub(super) async fn reads_run_beside_a_running_operation(h: Arc<SandboxHarness>) {
    let m = start(&h, 1).await;
    run_op(&h, &m, &write("op-1", &m, "a.txt", b"steady")).await;
    let op = exec("op-2", &m, SLEEP);
    dispatch_new(&h, &m, &op).await;
    assert_eq!(read_all(&h, &m, "a.txt").await, b"steady");
    let listing = port(h.backend.list(&m, &list_req("", 1, 500)).await);
    assert!(
        listing
            .entries
            .iter()
            .any(|entry| entry.path.as_str() == "a.txt")
    );
    let seen = port(h.backend.observe(&m, &op.id, 0).await);
    assert!(!seen.state.is_terminal(), "{:?}", seen.state);
    port(h.backend.cancel(&m, &op.id).await);
}

/// The volume is durable, the machine disposable.
pub(super) async fn files_persist_across_stop_and_new_generation(h: Arc<SandboxHarness>) {
    let old = start(&h, 1).await;
    run_op(&h, &old, &write("op-1", &old, "notes/a.txt", b"kept")).await;
    let proof = port(h.backend.stop_machine(&h.workspace_id, Generation(1)).await);
    assert_eq!(proof, StopProof::Stopped);
    let new = start(&h, 2).await;
    assert_eq!(read_all(&h, &new, "notes/a.txt").await, b"kept");
}

pub(super) async fn storage_usage_counts_written_bytes(h: Arc<SandboxHarness>) {
    let m = start(&h, 1).await;
    let before = port(h.backend.storage_usage(&h.workspace_id).await);
    let bytes = vec![b'z'; 64 * 1024];
    run_op(&h, &m, &write("op-1", &m, "big.txt", &bytes)).await;
    let after = port(h.backend.storage_usage(&h.workspace_id).await);
    assert!(
        after.used_bytes >= before.used_bytes + bytes.len() as u64,
        "{before:?} -> {after:?}"
    );
    assert!(after.quota_bytes > after.used_bytes && !after.is_exhausted());
}

fn home_req(path: &str) -> ReadFile {
    ReadFile {
        root: PathRoot::Home,
        ..read_req(path)
    }
}

/// `/home/agent` is its own tree on the durable volume: a path below it
/// never resolves below `/workspace`, and `reset_home` empties only it.
pub(super) async fn home_is_a_separate_tree_emptied_by_reset_home(h: Arc<SandboxHarness>) {
    let m = start(&h, 1).await;
    run_op(&h, &m, &write_home("op-1", &m, ".local/bin/tool", b"home")).await;
    run_op(&h, &m, &write("op-2", &m, "a.txt", b"kept")).await;
    let home = port(h.backend.read_file(&m, &home_req(".local/bin/tool")).await);
    assert_eq!(home.bytes, b"home");
    assert_not_found(h.backend.read_file(&m, &read_req(".local/bin/tool")).await);
    assert_not_found(h.backend.read_file(&m, &home_req("a.txt")).await);
    let listing = ListEntries {
        root: PathRoot::Home,
        ..list_req("", 1, 500)
    };
    let entries = port(h.backend.list(&m, &listing).await).entries;
    assert_eq!(entries, vec![entry(".local", EntryKind::Directory, None)]);
    assert_eq!(
        port(h.backend.stop_machine(&h.workspace_id, Generation(1)).await),
        StopProof::Stopped
    );
    port(h.backend.reset_home(&h.workspace_id).await);
    let next = start(&h, 2).await;
    assert_not_found(
        h.backend
            .read_file(&next, &home_req(".local/bin/tool"))
            .await,
    );
    assert_eq!(read_all(&h, &next, "a.txt").await, b"kept");
}

/// A binary file answers `binary` with no bytes, unless the read asks for
/// raw bytes (export).
pub(super) async fn raw_read_returns_binary_bytes(h: Arc<SandboxHarness>) {
    let m = start(&h, 1).await;
    let bytes = [0_u8, 1, 2, 0xff, b'\n'];
    run_op(&h, &m, &write("op-1", &m, "image.bin", &bytes)).await;
    let plain = port(h.backend.read_file(&m, &read_req("image.bin")).await);
    assert!(plain.binary && plain.bytes.is_empty(), "{plain:?}");
    let raw = ReadFile {
        raw: true,
        ..read_req("image.bin")
    };
    let chunk = port(h.backend.read_file(&m, &raw).await);
    assert_eq!(
        (chunk.binary, chunk.bytes.as_slice(), chunk.eof),
        (true, &bytes[..], true)
    );
    assert_eq!(chunk.size, bytes.len() as u64);
}
