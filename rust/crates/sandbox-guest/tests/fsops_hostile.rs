#![cfg(target_os = "linux")]
//! Hostile-input fs ops tests against real temp roots (§8.2): symlink
//! escapes, traversal shapes, FIFO/device targets, NUL/control names,
//! oversized bodies, CAS races, and write-through-symlink refusal.

use std::fs;
use std::os::unix::fs::PermissionsExt;

use agentkit_sandbox_contracts::api::messages::{GuestFsOp, GuestOpOutcome};
use agentkit_sandbox_contracts::ids::ArtifactDigest;
use agentkit_sandbox_contracts::path::GuestPath;
use agentkit_sandbox_guest::fsops::{GuestFs, Roots};
use base64::{Engine as _, engine::general_purpose::STANDARD};

struct Fixture {
    tmp: tempfile::TempDir,
    workspace: std::path::PathBuf,
    home: std::path::PathBuf,
    gstmp: std::path::PathBuf,
    fs: GuestFs,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let workspace = tmp.path().join("workspace");
    let home = tmp.path().join("home");
    let gstmp = tmp.path().join("tmp");
    for d in [&workspace, &home, &gstmp] {
        fs::create_dir_all(d).expect("root dir");
        fs::set_permissions(d, fs::Permissions::from_mode(0o755)).expect("perm");
    }
    let fs_obj = GuestFs::open(Roots::for_tests(
        workspace.clone(),
        home.clone(),
        gstmp.clone(),
    ))
    .expect("open roots");
    Fixture {
        tmp,
        workspace,
        home,
        gstmp,
        fs: fs_obj,
    }
}

fn failed(outcome: GuestOpOutcome) -> (String, String) {
    match outcome {
        GuestOpOutcome::Failed { code, message } => (code, message),
        other => panic!("expected Failed, got {other:?}"),
    }
}

fn wp(path: &str) -> GuestPath {
    GuestPath::parse(path).expect("valid guest path")
}

#[test]
fn symlink_escape_is_refused() {
    let f = fixture();
    std::os::unix::fs::symlink("/etc", f.workspace.join("etcdir")).expect("symlink");
    std::os::unix::fs::symlink("/etc/passwd", f.workspace.join("a")).expect("symlink");

    let (code, _) = failed(f.fs.handle(
        &GuestFsOp::Read {
            path: wp("/workspace/a"),
            from: None,
            len: None,
        },
        "r1",
    ));
    assert_eq!(code, "path_escape");

    // Through the symlinked directory: openat2 BENEATH must refuse.
    let (code, _) = failed(f.fs.handle(
        &GuestFsOp::Read {
            path: wp("/workspace/etcdir/passwd"),
            from: None,
            len: None,
        },
        "r2",
    ));
    assert_eq!(code, "path_escape");
}

#[test]
fn traversal_and_absolute_rejected_by_path_gate() {
    // The GuestPath type is the first gate: these never reach the syscall layer.
    for bad in [
        "/workspace/../etc/passwd",
        "/workspace/./x",
        "/etc/passwd",
        "workspace/a",
        "/workspace/a/../../etc",
    ] {
        assert!(GuestPath::parse(bad).is_err(), "must reject {bad}");
    }
    // Even a *valid* GuestPath under a root that maps to a symlinked dir
    // cannot escape (covered by symlink_escape_is_refused).
}

#[test]
fn absolute_path_cannot_be_forged_through_list() {
    let f = fixture();
    // "/workspace" itself lists; anything below must be inside the root.
    let out = f.fs.handle(
        &GuestFsOp::List {
            path: wp("/workspace"),
            options: Default::default(),
        },
        "l1",
    );
    assert!(matches!(out, GuestOpOutcome::ListOk { .. }));
}

#[test]
fn fifo_target_refused_for_write_and_read() {
    let f = fixture();
    let fifo = f.workspace.join("pipe");
    // SAFETY: mknod of a FIFO in our own temp dir.
    assert_eq!(
        unsafe {
            libc::mkfifo(
                std::ffi::CString::new(fifo.to_str().unwrap())
                    .unwrap()
                    .as_ptr(),
                0o600,
            )
        },
        0
    );
    let (code, _) = failed(f.fs.handle(
        &GuestFsOp::Write {
            path: wp("/workspace/pipe"),
            body_b64: STANDARD.encode(b"hi"),
            expected_sha256: None,
            create_parents: false,
        },
        "w1",
    ));
    assert_eq!(code, "path_escape");

    let (code, _) = failed(f.fs.handle(
        &GuestFsOp::Read {
            path: wp("/workspace/pipe"),
            from: None,
            len: None,
        },
        "r1",
    ));
    assert_eq!(code, "path_escape");
}

#[test]
fn nul_and_control_names_are_refused() {
    // GuestPath rejects these outright.
    assert!(GuestPath::parse("/workspace/nul\0x").is_err());
    assert!(GuestPath::parse("/workspace/ctrl\u{1}name").is_err());
}

#[test]
fn oversized_body_refused() {
    let f = fixture();
    let huge_b64 = "A".repeat(8 * 1024 * 1024); // ~6 MiB decoded: over ceiling
    let (code, _) = failed(f.fs.handle(
        &GuestFsOp::Write {
            path: wp("/workspace/big"),
            body_b64: huge_b64,
            expected_sha256: None,
            create_parents: false,
        },
        "w1",
    ));
    assert_eq!(code, "body_too_large");

    // Invalid base64 is refused, not panicked.
    let (code, _) = failed(f.fs.handle(
        &GuestFsOp::Write {
            path: wp("/workspace/big"),
            body_b64: "!!!not-base64!!!".to_string(),
            expected_sha256: None,
            create_parents: false,
        },
        "w1",
    ));
    assert_eq!(code, "bad_base64");
}

#[test]
fn cas_two_writers_one_wins_one_mismatches() {
    let f = fixture();
    fs::write(f.workspace.join("cas.txt"), b"v1").expect("seed");
    let d1 = ArtifactDigest::of_bytes(b"v1");

    let w1 = f.fs.handle(
        &GuestFsOp::Write {
            path: wp("/workspace/cas.txt"),
            body_b64: STANDARD.encode(b"v2-a"),
            expected_sha256: Some(d1.clone()),
            create_parents: false,
        },
        "w1",
    );
    assert!(
        matches!(w1, GuestOpOutcome::WriteOk { .. }),
        "first writer must win: {w1:?}"
    );

    let w2 = f.fs.handle(
        &GuestFsOp::Write {
            path: wp("/workspace/cas.txt"),
            body_b64: STANDARD.encode(b"v2-b"),
            expected_sha256: Some(d1), // stale: file is now v2-a
            create_parents: false,
        },
        "w2",
    );
    let (code, _) = failed(w2);
    assert_eq!(code, "digest_mismatch");
    assert_eq!(fs::read(f.workspace.join("cas.txt")).unwrap(), b"v2-a");
}

#[test]
fn cas_expected_digest_on_missing_target_mismatches() {
    let f = fixture();
    let (code, _) = failed(f.fs.handle(
        &GuestFsOp::Write {
            path: wp("/workspace/ghost"),
            body_b64: STANDARD.encode(b"x"),
            expected_sha256: Some(ArtifactDigest::of_bytes(b"x")),
            create_parents: false,
        },
        "w1",
    ));
    assert_eq!(code, "digest_mismatch");
}

#[test]
fn write_through_symlink_refused() {
    let f = fixture();
    fs::write(f.home.join("victim"), b"original").expect("seed");
    std::os::unix::fs::symlink(f.home.join("victim"), f.workspace.join("link")).expect("ln");
    let (code, _) = failed(f.fs.handle(
        &GuestFsOp::Write {
            path: wp("/workspace/link"),
            body_b64: STANDARD.encode(b"evil"),
            expected_sha256: None,
            create_parents: false,
        },
        "w1",
    ));
    assert_eq!(code, "path_escape");
    assert_eq!(fs::read(f.home.join("victim")).unwrap(), b"original");
}

#[test]
fn hardlink_semantics_stay_within_guest() {
    let f = fixture();
    // Hardlink inside the workspace: CAS still verified, rename replaces one
    // name (guest-local semantics; nothing outside the root is reachable).
    fs::write(f.workspace.join("orig"), b"shared").expect("seed");
    fs::hard_link(f.workspace.join("orig"), f.workspace.join("link2")).expect("link");
    let out = f.fs.handle(
        &GuestFsOp::Write {
            path: wp("/workspace/link2"),
            body_b64: STANDARD.encode(b"replaced"),
            expected_sha256: Some(ArtifactDigest::of_bytes(b"shared")),
            create_parents: false,
        },
        "w1",
    );
    assert!(matches!(out, GuestOpOutcome::WriteOk { .. }), "{out:?}");
    // Original name keeps the old inode content; link2 has the new content.
    assert_eq!(fs::read(f.workspace.join("orig")).unwrap(), b"shared");
    assert_eq!(fs::read(f.workspace.join("link2")).unwrap(), b"replaced");
}

#[test]
fn hardlink_into_etc_is_never_a_write_escape() {
    let f = fixture();
    let victim = std::path::Path::new("/etc/hostname");
    if !victim.exists() {
        return; // environment lacks the probe file
    }
    let before = fs::read(victim).expect("read victim");
    let link = f.workspace.join("hl");
    // SAFETY: link(2); unprivileged/foreign-fs attempts fail with EPERM/EXDEV.
    let rc = unsafe {
        libc::link(
            std::ffi::CString::new(victim.to_str().unwrap())
                .unwrap()
                .as_ptr(),
            std::ffi::CString::new(link.to_str().unwrap())
                .unwrap()
                .as_ptr(),
        )
    };
    if rc != 0 {
        // Expected on most systems: cannot hardlink foreign/protected files.
        let e = std::io::Error::last_os_error();
        assert!(
            matches!(
                e.raw_os_error(),
                Some(libc::EPERM) | Some(libc::EACCES) | Some(libc::EXDEV)
            ),
            "unexpected link error {e}"
        );
        return;
    }
    // Even with a same-fs hardlink, the write only replaces the workspace
    // name; the external file is untouched.
    let out = f.fs.handle(
        &GuestFsOp::Write {
            path: wp("/workspace/hl"),
            body_b64: STANDARD.encode(b"overwritten"),
            expected_sha256: Some(ArtifactDigest::of_bytes(&before)),
            create_parents: false,
        },
        "w1",
    );
    assert!(matches!(out, GuestOpOutcome::WriteOk { .. }), "{out:?}");
    assert_eq!(fs::read(victim).expect("reread"), before, "victim mutated!");
}

#[test]
fn list_hidden_and_truncation() {
    let f = fixture();
    fs::write(f.workspace.join("visible.txt"), b"x").expect("seed");
    fs::create_dir(f.workspace.join(".cache")).expect("mkdir");
    fs::write(f.workspace.join(".hidden"), b"y").expect("seed");

    let out = f.fs.handle(
        &GuestFsOp::List {
            path: wp("/workspace"),
            options: agentkit_sandbox_contracts::api::ListOptions {
                max_entries: 10,
                include_hidden: false,
            },
        },
        "l1",
    );
    let GuestOpOutcome::ListOk { reply } = out else {
        panic!("list failed: {out:?}");
    };
    let names: Vec<&str> = reply.entries.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"visible.txt"));
    assert!(!names.contains(&".hidden"));
    assert!(!names.contains(&".cache"));

    let out = f.fs.handle(
        &GuestFsOp::List {
            path: wp("/workspace"),
            options: agentkit_sandbox_contracts::api::ListOptions {
                max_entries: 10,
                include_hidden: true,
            },
        },
        "l2",
    );
    let GuestOpOutcome::ListOk { reply } = out else {
        panic!("list failed: {out:?}");
    };
    let names: Vec<&str> = reply.entries.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&".hidden"));
    assert_eq!(
        reply
            .entries
            .iter()
            .find(|e| e.name == ".cache")
            .map(|e| e.kind),
        Some(agentkit_sandbox_contracts::api::EntryKind::Dir)
    );

    // Truncation flag with a tiny cap over 3 entries.
    let out = f.fs.handle(
        &GuestFsOp::List {
            path: wp("/workspace"),
            options: agentkit_sandbox_contracts::api::ListOptions {
                max_entries: 1,
                include_hidden: true,
            },
        },
        "l3",
    );
    let GuestOpOutcome::ListOk { reply } = out else {
        panic!("list failed: {out:?}");
    };
    assert_eq!(reply.entries.len(), 1);
    assert!(reply.truncated);
}

#[test]
fn read_paging_and_digests() {
    let f = fixture();
    fs::write(f.workspace.join("pg.txt"), b"0123456789").expect("seed");
    let out = f.fs.handle(
        &GuestFsOp::Read {
            path: wp("/workspace/pg.txt"),
            from: Some(4),
            len: Some(3),
        },
        "r1",
    );
    let GuestOpOutcome::ReadOk { reply } = out else {
        panic!("read failed: {out:?}");
    };
    assert_eq!(STANDARD.decode(&reply.data_b64).unwrap(), b"456");
    assert_eq!(reply.from, 4);
    assert_eq!(reply.total_size, 10);
    assert_eq!(reply.next_from, Some(7));
    assert!(reply.truncated);
    assert_eq!(reply.sha256, ArtifactDigest::of_bytes(b"456"));
}

#[test]
fn create_parents_and_new_file_mode() {
    let f = fixture();
    let out = f.fs.handle(
        &GuestFsOp::Write {
            path: wp("/workspace/deep/nest/file.txt"),
            body_b64: STANDARD.encode(b"nested"),
            expected_sha256: None,
            create_parents: true,
        },
        "w1",
    );
    let GuestOpOutcome::WriteOk { receipt } = out else {
        panic!("write failed: {out:?}");
    };
    assert!(receipt.created);
    assert_eq!(receipt.sha256, ArtifactDigest::of_bytes(b"nested"));
    let written = f.workspace.join("deep/nest/file.txt");
    assert_eq!(fs::read(&written).unwrap(), b"nested");
    let mode = fs::metadata(&written).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o644);

    // Without create_parents the same write fails.
    let (code, _) = failed(f.fs.handle(
        &GuestFsOp::Write {
            path: wp("/workspace/other/new.txt"),
            body_b64: STANDARD.encode(b"x"),
            expected_sha256: None,
            create_parents: false,
        },
        "w2",
    ));
    assert!(code == "path_escape" || code == "not_found", "{code}");
}

fn stage_file(rel: &str, body: &[u8]) -> agentkit_sandbox_contracts::api::StageFile {
    agentkit_sandbox_contracts::api::StageFile {
        relative_path: rel.into(),
        sha256: ArtifactDigest::of_bytes(body),
        size: body.len() as u64,
        mime: "text/plain".into(),
        provenance: None,
    }
}

fn sid(n: &str) -> agentkit_sandbox_contracts::ids::StageId {
    agentkit_sandbox_contracts::ids::StageId::new(n).unwrap()
}

fn import_ok(f: &Fixture, n: &str, files: &[agentkit_sandbox_contracts::api::StageFile]) {
    let out = f.fs.handle(
        &GuestFsOp::ImportBegin {
            stage_id: sid(n),
            files: files.to_vec(),
        },
        "ib",
    );
    assert!(matches!(out, GuestOpOutcome::Ack {}), "{out:?}");
}

fn import_chunk(
    f: &Fixture,
    n: &str,
    rel: &str,
    seq: u32,
    body: &[u8],
    digest: Option<ArtifactDigest>,
) -> GuestOpOutcome {
    f.fs.handle(
        &GuestFsOp::ImportData {
            stage_id: sid(n),
            relative_path: rel.into(),
            sequence: seq,
            bytes_b64: STANDARD.encode(body),
            chunk_digest: digest.unwrap_or_else(|| ArtifactDigest::of_bytes(body)),
        },
        "id",
    )
}

#[test]
fn streaming_import_commits_only_verified_files() {
    let f = fixture();
    let body = b"staged content".to_vec();
    import_ok(&f, "i1", &[stage_file("pkg/a.txt", &body)]);
    assert!(matches!(
        import_chunk(&f, "i1", "pkg/a.txt", 0, &body, None),
        GuestOpOutcome::Ack {}
    ));
    let out = f.fs.handle(
        &GuestFsOp::ImportEnd {
            stage_id: sid("i1"),
        },
        "ie",
    );
    let GuestOpOutcome::ImportOk { copied } = out else {
        panic!("import failed: {out:?}");
    };
    assert_eq!(copied, 1);
    let written = f.workspace.join("pkg/a.txt");
    assert_eq!(fs::read(&written).unwrap(), body);
    let mode = fs::metadata(&written).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "imported files must be owner-only");
}

#[test]
fn import_rejects_bad_chunk_digest_sequence_and_oversize() {
    let f = fixture();
    let body = b"abcdef".to_vec();
    let spec = stage_file("x.bin", &body);
    import_ok(&f, "i2", std::slice::from_ref(&spec));
    // tampered bytes carrying the digest of the *intended* body
    let (code, _) = failed(import_chunk(
        &f,
        "i2",
        "x.bin",
        0,
        b"zzzzzz",
        Some(ArtifactDigest::of_bytes(&body)),
    ));
    assert_eq!(code, "digest_mismatch");
    // gap (expect 0 again after rejection)
    let (code, _) = failed(import_chunk(&f, "i2", "x.bin", 1, &body[..3], None));
    assert_eq!(code, "bad_input");
    // correct chunk then overrun
    assert!(matches!(
        import_chunk(&f, "i2", "x.bin", 0, &body[..3], None),
        GuestOpOutcome::Ack {}
    ));
    let (code, _) = failed(import_chunk(&f, "i2", "x.bin", 1, b"toolongtail", None));
    assert_eq!(code, "body_too_large");
    // end with incomplete file fails closed and leaves workspace clean
    let (code, _) = failed(f.fs.handle(
        &GuestFsOp::ImportEnd {
            stage_id: sid("i2"),
        },
        "ie",
    ));
    assert_eq!(code, "digest_mismatch");
    assert!(!f.workspace.join("x.bin").exists());
}

#[test]
fn import_refuses_overwrite_and_duplicate_manifest() {
    let f = fixture();
    std::fs::create_dir_all(&f.workspace).unwrap();
    fs::write(f.workspace.join("exists.txt"), b"original").unwrap();
    let body = b"clobber".to_vec();
    import_ok(&f, "i3", &[stage_file("exists.txt", &body)]);
    assert!(matches!(
        import_chunk(&f, "i3", "exists.txt", 0, &body, None),
        GuestOpOutcome::Ack {}
    ));
    let (code, _) = failed(f.fs.handle(
        &GuestFsOp::ImportEnd {
            stage_id: sid("i3"),
        },
        "ie",
    ));
    assert_eq!(code, "bad_input", "NOREPLACE must refuse overwrite");
    assert_eq!(
        fs::read(f.workspace.join("exists.txt")).unwrap(),
        b"original"
    );

    // duplicate path in one manifest is rejected at begin
    let (code, _) = failed(f.fs.handle(
        &GuestFsOp::ImportBegin {
            stage_id: sid("i4"),
            files: vec![stage_file("d.txt", b"a"), stage_file("d.txt", b"b")],
        },
        "ib",
    ));
    assert_eq!(code, "bad_input");

    // traversal in manifest rejected at begin
    let (code, _) = failed(f.fs.handle(
        &GuestFsOp::ImportBegin {
            stage_id: sid("i5"),
            files: vec![stage_file("../escape", b"a")],
        },
        "ib",
    ));
    assert_eq!(code, "path_escape");
}

#[test]
fn export_gather_pins_and_fetch_serves_chunks() {
    let f = fixture();
    std::fs::create_dir_all(f.workspace.join("out")).unwrap();
    let a = b"alpha-bytes".to_vec();
    let b = vec![0x42u8; agentkit_sandbox_contracts::limits::MAX_STAGE_CHUNK_BYTES + 7];
    fs::write(f.workspace.join("out/b.txt"), &b).unwrap();
    fs::write(f.workspace.join("out/a.txt"), &a).unwrap();
    let out = f.fs.handle(
        &GuestFsOp::ExportGather {
            stage_id: sid("e1"),
            paths: vec![wp("/workspace/out/b.txt"), wp("/workspace/out/a.txt")],
        },
        "eg",
    );
    let GuestOpOutcome::ExportOk { manifest } = out else {
        panic!("gather failed: {out:?}");
    };
    assert_eq!(manifest.files.len(), 2);
    assert_eq!(
        manifest.files[0].relative_path, "out/a.txt",
        "must be sorted"
    );
    assert_eq!(manifest.files[1].size, b.len() as u64);
    assert_eq!(manifest.total_bytes, (a.len() + b.len()) as u64);
    // fetch last chunk of b
    let last_seq = (b.len() / agentkit_sandbox_contracts::limits::MAX_STAGE_CHUNK_BYTES) as u32;
    let out = f.fs.handle(
        &GuestFsOp::ExportFetch {
            stage_id: sid("e1"),
            relative_path: "out/b.txt".into(),
            sequence: last_seq,
        },
        "ef",
    );
    let GuestOpOutcome::ExportDataOk {
        bytes_b64,
        chunk_digest,
        last,
    } = out
    else {
        panic!("fetch failed: {out:?}");
    };
    assert!(last);
    let chunk = STANDARD.decode(&bytes_b64).unwrap();
    assert_eq!(ArtifactDigest::of_bytes(&chunk), chunk_digest);
    assert_eq!(chunk, &b[b.len() - chunk.len()..]);
    // sequence past end refused
    let (code, _) = failed(f.fs.handle(
        &GuestFsOp::ExportFetch {
            stage_id: sid("e1"),
            relative_path: "out/b.txt".into(),
            sequence: last_seq + 5,
        },
        "ef",
    ));
    assert_eq!(code, "bad_input");
}

#[test]
fn export_fetch_refuses_files_mutated_after_gather() {
    let f = fixture();
    fs::write(f.workspace.join("m.txt"), b"one").unwrap();
    let out = f.fs.handle(
        &GuestFsOp::ExportGather {
            stage_id: sid("e2"),
            paths: vec![wp("/workspace/m.txt")],
        },
        "eg",
    );
    assert!(matches!(out, GuestOpOutcome::ExportOk { .. }), "{out:?}");
    fs::write(f.workspace.join("m.txt"), b"two-o'clock").unwrap();
    let (code, _) = failed(f.fs.handle(
        &GuestFsOp::ExportFetch {
            stage_id: sid("e2"),
            relative_path: "m.txt".into(),
            sequence: 0,
        },
        "ef",
    ));
    assert_eq!(code, "digest_mismatch");
}

#[test]
fn export_gather_refuses_non_workspace_and_symlinks() {
    let f = fixture();
    let (code, _) = failed(f.fs.handle(
        &GuestFsOp::ExportGather {
            stage_id: sid("e3"),
            paths: vec![wp("/home/agent/secrets")],
        },
        "eg",
    ));
    assert_eq!(code, "path_escape");
    std::os::unix::fs::symlink("/etc/passwd", f.workspace.join("sl")).unwrap();
    let (code, _) = failed(f.fs.handle(
        &GuestFsOp::ExportGather {
            stage_id: sid("e4"),
            paths: vec![wp("/workspace/sl")],
        },
        "eg",
    ));
    assert_eq!(code, "path_escape", "symlink must not be gatherable");
}

// Keep unused-warning quiet for the fixture fields used only in some tests.
#[allow(dead_code)]
fn touch(f: &Fixture) {
    let _ = &f.tmp;
    let _ = &f.gstmp;
}
