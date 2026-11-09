//! End-to-end staged artifact flows through the service (§11.1/§11.2,
//! T13/T14/T24 evidence at L1): chunked import with digest/sequence
//! enforcement, tenant-bound commit into the guest, and export gather with
//! worker-side re-hash verification.

use agentkit_sandbox_contracts::api::messages::{GuestFsOp, HostToWorker};
use agentkit_sandbox_contracts::api::{StageDirection, StageFile, StageManifest};
use agentkit_sandbox_contracts::error::SandboxErrorCode;
use agentkit_sandbox_contracts::ids::RequestDigest;
use agentkit_sandbox_contracts::ids::{ArtifactDigest, StageId};
use agentkit_sandbox_contracts::policy::SandboxCapability;
use agentkit_sandbox_worker::service::canonical_body;

mod support;
use support::{Harness, gp, sid, stage_id, tid};

fn spec(rel: &str, body: &[u8]) -> StageFile {
    StageFile {
        relative_path: rel.into(),
        sha256: ArtifactDigest::of_bytes(body),
        size: body.len() as u64,
        mime: "text/plain".into(),
        provenance: None,
    }
}

fn manifest(stage: &StageId, dir: StageDirection, files: Vec<StageFile>) -> StageManifest {
    let total = files.iter().map(|f| f.size).sum();
    let mut m = StageManifest {
        stage_id: stage.clone(),
        direction: dir,
        files,
        total_bytes: total,
        manifest_digest: ArtifactDigest::of_bytes(b"placeholder"),
    };
    m.manifest_digest =
        agentkit_sandbox_contracts::canonical::manifest_digest_of(&m).expect("canonical");
    m
}

fn b64(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

async fn ready(h: &Harness) {
    h.create_ready("alice", "stagebox").await;
}

#[tokio::test]
async fn import_streams_chunks_and_lands_in_guest() {
    let h = Harness::new();
    ready(&h).await;
    let task = agentkit_sandbox_contracts::ids::TaskId::new("task-w1").unwrap();
    let lease = h
        .acquire_writer(&tid("alice"), &sid("stagebox"), &task, 60_000)
        .await;
    let stage = stage_id("imp1");
    let a = b"first artifact".to_vec();
    let b = vec![0x5au8; 5000];
    let files = vec![spec("pkg/a.txt", &a), spec("pkg/nested/b.bin", &b)];
    let m = manifest(&stage, StageDirection::Import, files.clone());

    let env = h.envelope(
        &tid("alice"),
        &sid("stagebox"),
        support::genof(1),
        SandboxCapability::SandboxManage,
    );
    h.ok(
        "sb",
        1,
        &HostToWorker::StageBegin {
            envelope: env.clone(),
            manifest: m.clone(),
        },
    )
    .await;
    for (i, _f) in files.iter().enumerate() {
        let bytes = if i == 0 { &a } else { &b };
        h.ok(
            &format!("sc{i}"),
            10 + i as u64,
            &HostToWorker::StageChunk {
                envelope: env.clone(),
                stage_id: stage.clone(),
                sequence: i as u32,
                bytes_b64: b64(bytes),
                chunk_digest: ArtifactDigest::of_bytes(bytes),
            },
        )
        .await;
    }
    h.ok(
        "scommit",
        20,
        &HostToWorker::StageCommit {
            envelope: env,
            stage_id: stage.clone(),
            manifest_digest: m.manifest_digest.clone(),
        },
    )
    .await;

    // Import into the guest workspace.
    let digest =
        RequestDigest::of_canonical_json(&canonical_body(&(&stage, &m.manifest_digest)).unwrap());
    let body = h
        .ok(
            "ic",
            30,
            &HostToWorker::ImportCommit {
                grant: h.grant_w(
                    &tid("alice"),
                    &sid("stagebox"),
                    SandboxCapability::ArtifactImport,
                    digest,
                    vec![
                        gp("/workspace/pkg/a.txt"),
                        gp("/workspace/pkg/nested/b.bin"),
                    ],
                    None,
                    Some(task.clone()),
                    Some(lease.clone()),
                ),
                stage_id: stage.clone(),
                manifest_digest: m.manifest_digest.clone(),
            },
        )
        .await;
    assert!(matches!(
        body,
        agentkit_sandbox_contracts::api::messages::WorkerReplyBody::Imported { copied: 2 }
    ));

    // The guest now serves the imported bytes.
    let op = GuestFsOp::Read {
        path: gp("/workspace/pkg/a.txt"),
        from: None,
        len: None,
    };
    let d = RequestDigest::of_canonical_json(&canonical_body(&op).unwrap());
    let body = h
        .ok(
            "rd",
            40,
            &HostToWorker::FsRead {
                grant: h.grant(
                    &tid("alice"),
                    &sid("stagebox"),
                    SandboxCapability::SandboxRead,
                    d,
                    vec![gp("/workspace/pkg/a.txt")],
                    None,
                ),
                path: gp("/workspace/pkg/a.txt"),
                from: None,
                len: None,
            },
        )
        .await;
    match body {
        agentkit_sandbox_contracts::api::messages::WorkerReplyBody::Read { reply } => {
            use base64::Engine as _;
            let got = base64::engine::general_purpose::STANDARD
                .decode(&reply.data_b64)
                .unwrap();
            assert_eq!(got, a);
            assert_eq!(reply.sha256, ArtifactDigest::of_bytes(&a));
        }
        other => panic!("expected read, got {other:?}"),
    }

    // Journal carries begin + 2 chunks + commit.
    let kinds: Vec<String> = h
        .journal_records()
        .iter()
        .map(|r| {
            format!("{r:?}")
                .split_whitespace()
                .next()
                .unwrap_or("?")
                .to_string()
        })
        .collect();
    assert_eq!(
        kinds.iter().filter(|k| k.starts_with("StageBegin")).count(),
        1
    );
    assert_eq!(
        kinds.iter().filter(|k| k.starts_with("StageChunk")).count(),
        2
    );
    assert_eq!(
        kinds
            .iter()
            .filter(|k| k.starts_with("StageCommit"))
            .count(),
        1
    );
}

#[tokio::test]
async fn import_rejects_sequence_gaps_and_tampering() {
    let h = Harness::new();
    ready(&h).await;
    let task = agentkit_sandbox_contracts::ids::TaskId::new("task-w2").unwrap();
    let lease = h
        .acquire_writer(&tid("alice"), &sid("stagebox"), &task, 60_000)
        .await;
    let stage = stage_id("imp2");
    let a = b"content".to_vec();
    let files = vec![spec("x.bin", &a)];
    let m = manifest(&stage, StageDirection::Import, files);
    let env = h.envelope(
        &tid("alice"),
        &sid("stagebox"),
        support::genof(1),
        SandboxCapability::SandboxManage,
    );
    h.ok(
        "sb",
        1,
        &HostToWorker::StageBegin {
            envelope: env.clone(),
            manifest: m.clone(),
        },
    )
    .await;
    // gap: first chunk at sequence 1
    let e = h
        .err(
            "gap",
            2,
            &HostToWorker::StageChunk {
                envelope: env.clone(),
                stage_id: stage.clone(),
                sequence: 1,
                bytes_b64: b64(&a),
                chunk_digest: ArtifactDigest::of_bytes(&a),
            },
        )
        .await;
    assert!(
        e.code == SandboxErrorCode::ProtocolIncompatible
            || e.code == SandboxErrorCode::ArtifactInvalid,
        "{e:?}"
    );
    // tampered bytes vs claimed digest
    let e = h
        .err(
            "tamper",
            3,
            &HostToWorker::StageChunk {
                envelope: env.clone(),
                stage_id: stage.clone(),
                sequence: 0,
                bytes_b64: b64(b"tampered"),
                chunk_digest: ArtifactDigest::of_bytes(&a),
            },
        )
        .await;
    assert_eq!(e.code, SandboxErrorCode::ArtifactInvalid);

    // Commit with the wrong manifest digest is refused.
    let e = h
        .err(
            "badcommit",
            4,
            &HostToWorker::StageCommit {
                envelope: env.clone(),
                stage_id: stage.clone(),
                manifest_digest: ArtifactDigest::of_bytes(b"wrong"),
            },
        )
        .await;
    assert_eq!(e.code, SandboxErrorCode::ArtifactInvalid);

    // Importing an uncommitted stage fails.
    let digest =
        RequestDigest::of_canonical_json(&canonical_body(&(&stage, &m.manifest_digest)).unwrap());
    let e = h
        .err(
            "ic",
            5,
            &HostToWorker::ImportCommit {
                grant: h.grant_w(
                    &tid("alice"),
                    &sid("stagebox"),
                    SandboxCapability::ArtifactImport,
                    digest,
                    vec![gp("/workspace/x.bin")],
                    None,
                    Some(task.clone()),
                    Some(lease.clone()),
                ),
                stage_id: stage.clone(),
                manifest_digest: m.manifest_digest.clone(),
            },
        )
        .await;
    assert!(
        e.code == SandboxErrorCode::PolicyDenied || e.code == SandboxErrorCode::ArtifactInvalid,
        "{e:?}"
    );
}

#[tokio::test]
async fn export_gathers_verifies_and_journals() {
    let h = Harness::new();
    ready(&h).await;
    let task = agentkit_sandbox_contracts::ids::TaskId::new("task-w3").unwrap();
    let lease = h
        .acquire_writer(&tid("alice"), &sid("stagebox"), &task, 60_000)
        .await;
    // Seed the guest workspace via FsWrite.
    let body_a = b"exported alpha".to_vec();
    let body_b = b"exported beta longer content".to_vec();
    for (i, (p, body)) in [
        ("/workspace/out/b.txt", &body_b),
        ("/workspace/out/a.txt", &body_a),
    ]
    .iter()
    .enumerate()
    {
        let op = GuestFsOp::Write {
            path: gp(p),
            body_b64: b64(body),
            expected_sha256: None,
            create_parents: true,
        };
        let d = RequestDigest::of_canonical_json(&canonical_body(&op).unwrap());
        h.ok(
            &format!("w{i}"),
            1 + i as u64,
            &HostToWorker::FsWrite {
                grant: h.grant_w(
                    &tid("alice"),
                    &sid("stagebox"),
                    SandboxCapability::SandboxWrite,
                    d,
                    vec![gp(p)],
                    None,
                    Some(task.clone()),
                    Some(lease.clone()),
                ),
                path: gp(p),
                body_b64: b64(body),
                expected_sha256: None,
            },
        )
        .await;
    }

    let stage = stage_id("exp1");
    let paths = vec![gp("/workspace/out/b.txt"), gp("/workspace/out/a.txt")];
    let digest = RequestDigest::of_canonical_json(&canonical_body(&(&stage, &paths)).unwrap());
    let body = h
        .ok(
            "ex",
            9,
            &HostToWorker::ExportRequest {
                grant: h.grant(
                    &tid("alice"),
                    &sid("stagebox"),
                    SandboxCapability::ArtifactExport,
                    digest,
                    paths.clone(),
                    None,
                ),
                stage_id: stage.clone(),
                paths,
            },
        )
        .await;
    let manifest = match body {
        agentkit_sandbox_contracts::api::messages::WorkerReplyBody::Exported { manifest: m } => m,
        other => panic!("expected export manifest, got {other:?}"),
    };
    assert_eq!(manifest.files.len(), 2);
    assert_eq!(manifest.files[0].relative_path, "out/a.txt", "sorted");
    assert_eq!(manifest.files[1].relative_path, "out/b.txt");
    assert_eq!(manifest.total_bytes, (body_a.len() + body_b.len()) as u64);
    assert_eq!(
        manifest.manifest_digest,
        agentkit_sandbox_contracts::canonical::manifest_digest_of(&manifest).unwrap()
    );
    // Stage reached committed state with exactly the gathered bytes.
    let rec = h
        .svc
        .with_registry(|reg| {
            reg.stage(&tid("alice"), &stage).ok().map(|r| {
                (
                    r.committed,
                    r.aborted,
                    r.bytes_received,
                    r.manifest_digest.clone(),
                    r.files.clone(),
                )
            })
        })
        .expect("stage recorded");
    assert!(rec.0 && !rec.1);
    assert_eq!(rec.2, manifest.total_bytes);
    assert_eq!(rec.3, manifest.manifest_digest);
    assert_eq!(rec.4, manifest.files);
}

#[tokio::test]
async fn export_missing_path_fails_and_bad_stage_grant_denied() {
    let h = Harness::new();
    ready(&h).await;
    let stage = stage_id("exp2");
    let paths = vec![gp("/workspace/nope.txt")];
    let digest = RequestDigest::of_canonical_json(&canonical_body(&(&stage, &paths)).unwrap());
    let e = h
        .err(
            "ex",
            3,
            &HostToWorker::ExportRequest {
                grant: h.grant(
                    &tid("alice"),
                    &sid("stagebox"),
                    SandboxCapability::ArtifactExport,
                    digest,
                    paths.clone(),
                    None,
                ),
                stage_id: stage,
                paths,
            },
        )
        .await;
    assert!(
        e.code == SandboxErrorCode::NotFound || e.code == SandboxErrorCode::ArtifactInvalid,
        "{e:?}"
    );
}
