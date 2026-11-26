//! Files over the protocol: atomic digest-checked writes, bounded reads
//! and listings, the two roots, usage, and reads beside a running command.

mod support;

use std::collections::BTreeSet;
use std::fs::Permissions;
use std::os::unix::fs::{MetadataExt, PermissionsExt};

use agentkit_sandbox_proto::guest::{
    Entry, EntryKind, GuestErrorCode, ListRequest, OperationState, PathRoot, ReadRequest, Reply,
    Request,
};
use support::{
    Guest, assert_new, exec_request, failed, path_exists, read_request, start, write_request,
};

const ENV: &str = "PATH=/usr/bin:/bin\n";
const WS: PathRoot = PathRoot::Workspace;

async fn write_ok(host: &mut support::Host, op: &str, root: PathRoot, path: &str, bytes: &[u8]) {
    let ack = host
        .write(write_request(op, root, path, bytes, bytes), bytes)
        .await;
    assert_new(&ack);
    assert_eq!(
        host.finish(op).await.state,
        OperationState::Succeeded { exit: 0 }
    );
}

async fn read_all(host: &mut support::Host, root: PathRoot, path: &str) -> Vec<u8> {
    host.read(read_request(root, path)).await.unwrap().1
}

fn list(path: &str, depth: u8, max_entries: u32) -> Request {
    Request::List(ListRequest {
        root: WS,
        path: path.to_owned(),
        depth,
        max_entries,
    })
}

#[tokio::test]
async fn write_then_read_round_trips_and_replaces() {
    let guest = Guest::new(ENV);
    let mut host = start(&guest).await;
    write_ok(&mut host, "op-1", WS, "dir/a.txt", b"hello world").await;
    let (header, bytes) = host.read(read_request(WS, "dir/a.txt")).await.unwrap();
    assert_eq!(bytes, b"hello world");
    assert_eq!((header.size, header.binary, header.eof), (11, false, true));
    let part = ReadRequest {
        offset: 6,
        limit_bytes: 3,
        ..read_request(WS, "dir/a.txt")
    };
    let (header, bytes) = host.read(part).await.unwrap();
    assert_eq!((bytes.as_slice(), header.eof), (&b"wor"[..], false));
    write_ok(&mut host, "op-2", WS, "dir/a.txt", b"bye").await;
    assert_eq!(read_all(&mut host, WS, "dir/a.txt").await, b"bye");
    let replay = host
        .write(
            write_request("op-2", WS, "dir/a.txt", b"bye", b"bye"),
            b"bye",
        )
        .await;
    assert!(
        matches!(replay, Reply::Accepted { existing: true, .. }),
        "{replay:?}"
    );
    let names: Vec<_> = std::fs::read_dir(guest.workspace.join("dir"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(names, ["a.txt"], "no temp file is left behind");
}

#[tokio::test]
async fn planted_temp_names_do_not_block_writes() {
    let guest = Guest::new(ENV);
    // Every name the old scheme (`.sandboxd-<pid>-<n>.tmp`) would try.
    let pid = std::process::id();
    let planted: Vec<_> = (0..16)
        .map(|n| guest.workspace.join(format!(".sandboxd-{pid}-{n}.tmp")))
        .collect();
    for path in &planted {
        std::fs::write(path, b"planted").unwrap();
    }
    let mut host = start(&guest).await;
    write_ok(&mut host, "op-1", WS, "a.txt", b"landed").await;
    assert_eq!(read_all(&mut host, WS, "a.txt").await, b"landed");
    for path in &planted {
        assert_eq!(std::fs::read(path).unwrap(), b"planted");
    }
}

#[tokio::test]
async fn a_write_with_the_wrong_digest_changes_nothing() {
    let guest = Guest::new(ENV);
    let mut host = start(&guest).await;
    write_ok(&mut host, "op-1", WS, "a.txt", b"v1").await;
    let bad = write_request("op-2", WS, "a.txt", b"v2", b"something else");
    assert_new(&host.write(bad, b"v2").await);
    assert_eq!(host.finish("op-2").await.state, failed(1));
    assert_eq!(read_all(&mut host, WS, "a.txt").await, b"v1");
    let fresh = write_request("op-3", WS, "b.txt", b"v2", b"something else");
    assert_new(&host.write(fresh, b"v2").await);
    host.finish("op-3").await;
    assert!(!path_exists(&guest.workspace.join("b.txt")));
    assert_eq!(std::fs::read_dir(&guest.workspace).unwrap().count(), 1);
}

#[tokio::test]
async fn a_stream_shorter_than_its_size_is_refused() {
    let guest = Guest::new(ENV);
    let mut host = start(&guest).await;
    let mut req = write_request("op-1", WS, "a.txt", b"abc", b"abc");
    req.size = 5;
    let reply = host.write(req, b"abc").await;
    assert!(
        matches!(&reply, Reply::Error(error) if error.code == GuestErrorCode::SizeMismatch),
        "{reply:?}"
    );
    assert_eq!(host.status("op-1").await, Reply::State { state: failed(1) });
    assert!(!path_exists(&guest.workspace.join("a.txt")));
    // The channel is still in step: the next request is answered.
    assert_eq!(host.status("never").await, Reply::Unknown);
}

#[tokio::test]
async fn writes_do_not_follow_links_or_replace_directories() {
    let guest = Guest::new(ENV);
    let outside = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), guest.workspace.join("escape")).unwrap();
    std::fs::create_dir(guest.workspace.join("dir")).unwrap();
    let mut host = start(&guest).await;
    let through = write_request("op-1", WS, "escape/x.txt", b"x", b"x");
    assert_new(&host.write(through, b"x").await);
    assert_eq!(host.finish("op-1").await.state, failed(1));
    assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    let over_dir = write_request("op-2", WS, "dir", b"x", b"x");
    assert_new(&host.write(over_dir, b"x").await);
    assert_eq!(host.finish("op-2").await.state, failed(1));
    assert!(guest.workspace.join("dir").is_dir());
    let invalid = write_request("op-3", WS, "", b"x", b"x");
    let reply = host.write(invalid, b"x").await;
    assert!(matches!(reply, Reply::Error(_)), "{reply:?}");
    assert_eq!(host.status("op-3").await, Reply::Unknown);
}

#[tokio::test]
async fn reads_refuse_missing_paths_directories_and_large_files() {
    let guest = Guest::new(ENV);
    let mut host = start(&guest).await;
    write_ok(&mut host, "op-1", WS, "dir/a.txt", b"hello world").await;
    let code = |result: Result<_, agentkit_sandbox_proto::guest::GuestError>| {
        result.map(|_| ()).unwrap_err().code
    };
    let missing = host.read(read_request(WS, "missing.txt")).await;
    assert_eq!(code(missing), GuestErrorCode::NotFound);
    let dir = host.read(read_request(WS, "dir")).await;
    assert_eq!(code(dir), GuestErrorCode::NotARegularFile);
    let capped = ReadRequest {
        max_file_bytes: 4,
        ..read_request(WS, "dir/a.txt")
    };
    let large = host.read(capped).await;
    assert_eq!(code(large), GuestErrorCode::FileTooLarge { size: 11 });
}

#[tokio::test]
async fn binary_files_have_bytes_only_when_raw() {
    let guest = Guest::new(ENV);
    let mut host = start(&guest).await;
    let content = [0_u8, 1, 2, 0xff, b'\n'];
    write_ok(&mut host, "op-1", WS, "image.bin", &content).await;
    let (plain, bytes) = host.read(read_request(WS, "image.bin")).await.unwrap();
    assert!(plain.binary && bytes.is_empty());
    let raw = ReadRequest {
        raw: true,
        ..read_request(WS, "image.bin")
    };
    let (header, bytes) = host.read(raw).await.unwrap();
    assert_eq!(
        (header.binary, bytes.as_slice(), header.eof),
        (true, &content[..], true)
    );
}

#[tokio::test]
async fn listings_honour_depth_and_limit() {
    let guest = Guest::new(ENV);
    let mut host = start(&guest).await;
    for (n, path) in ["tree/a.txt", "tree/d/b.txt", "tree/d/e/c.txt"]
        .iter()
        .enumerate()
    {
        write_ok(&mut host, &format!("op-{n}"), WS, path, b"abc").await;
    }
    let paths = |reply: Reply| match reply {
        Reply::Entries { entries, truncated } => {
            let set: BTreeSet<String> = entries.into_iter().map(|entry| entry.path).collect();
            (set, truncated)
        }
        other => panic!("expected entries, got {other:?}"),
    };
    let (one, cut) = paths(host.request(list("tree", 1, 500)).await);
    assert_eq!(one, BTreeSet::from(["tree/a.txt".into(), "tree/d".into()]));
    assert!(!cut);
    let (all, _) = paths(host.request(list("tree", 4, 500)).await);
    assert_eq!(all.len(), 5);
    let (two, cut) = paths(host.request(list("tree", 4, 2)).await);
    assert_eq!((two.len(), cut), (2, true));
    let absent = host.request(list("absent", 1, 10)).await;
    assert!(matches!(&absent, Reply::Error(error) if error.code == GuestErrorCode::NotFound));
}

#[tokio::test]
async fn home_is_its_own_tree() {
    let guest = Guest::new(ENV);
    let mut host = start(&guest).await;
    write_ok(
        &mut host,
        "op-1",
        PathRoot::Home,
        ".local/bin/tool",
        b"home",
    )
    .await;
    write_ok(&mut host, "op-2", WS, "a.txt", b"kept").await;
    assert_eq!(
        read_all(&mut host, PathRoot::Home, ".local/bin/tool").await,
        b"home"
    );
    let wrong_tree = host.read(read_request(WS, ".local/bin/tool")).await;
    assert!(wrong_tree.is_err());
    let listing = host
        .request(Request::List(ListRequest {
            root: PathRoot::Home,
            path: String::new(),
            depth: 1,
            max_entries: 500,
        }))
        .await;
    let expected = vec![Entry {
        path: ".local".into(),
        kind: EntryKind::Directory,
        size: None,
    }];
    assert_eq!(
        listing,
        Reply::Entries {
            entries: expected,
            truncated: false
        }
    );
}

#[tokio::test]
async fn usage_counts_written_bytes() {
    let guest = Guest::new(ENV);
    let mut host = start(&guest).await;
    let usage = |reply: Reply| match reply {
        Reply::Usage {
            workspace_bytes,
            home_bytes,
            complete: true,
        } => (workspace_bytes, home_bytes),
        other => panic!("expected complete usage, got {other:?}"),
    };
    let before = usage(host.request(Request::Usage).await);
    write_ok(&mut host, "op-1", WS, "big.txt", &vec![b'z'; 64 * 1024]).await;
    write_ok(&mut host, "op-2", PathRoot::Home, "h.txt", b"1234").await;
    let after = usage(host.request(Request::Usage).await);
    assert!(after.0 >= before.0 + 64 * 1024, "{before:?} -> {after:?}");
    // A file counts its allocated space when that is larger than its size.
    let home_file = std::fs::metadata(guest.home.join("h.txt")).unwrap();
    let counted = home_file.len().max(home_file.blocks() * 512);
    assert!(counted >= 4);
    assert_eq!(after.1, before.1 + counted);
}

#[tokio::test]
async fn usage_says_when_part_of_a_tree_could_not_be_measured() {
    if agentkit_sys::effective_uid().unwrap() == 0 {
        return;
    }
    let guest = Guest::new(ENV);
    let mut host = start(&guest).await;
    write_ok(
        &mut host,
        "op-1",
        WS,
        "hidden/big.txt",
        &vec![b'z'; 1 << 20],
    )
    .await;
    let locked = guest.workspace.join("hidden");
    std::fs::set_permissions(&locked, Permissions::from_mode(0o000)).unwrap();
    let reply = host.request(Request::Usage).await;
    std::fs::set_permissions(&locked, Permissions::from_mode(0o755)).unwrap();
    assert!(
        matches!(
            reply,
            Reply::Usage {
                complete: false,
                ..
            }
        ),
        "{reply:?}"
    );
}

#[tokio::test]
async fn reads_run_beside_a_running_command() {
    let guest = Guest::new(ENV);
    let mut host = start(&guest).await;
    write_ok(&mut host, "op-1", WS, "a.txt", b"steady").await;
    assert_new(&host.exec(exec_request("op-2", "sleep 3600")).await);
    assert_eq!(read_all(&mut host, WS, "a.txt").await, b"steady");
    let listing = host.request(list("", 1, 500)).await;
    assert!(
        matches!(&listing, Reply::Entries { entries, .. } if entries.iter().any(|e| e.path == "a.txt"))
    );
    assert_eq!(
        host.status("op-2").await,
        Reply::State {
            state: OperationState::Running
        }
    );
    host.request(Request::Cancel { op: "op-2".into() }).await;
    assert_eq!(host.finish("op-2").await.state, OperationState::Cancelled);
}
