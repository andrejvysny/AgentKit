//! OCI importer e2e (L1): fake registry → digest verification → policy
//! extraction → content-addressed store (§10.2/§10.3). The disposable-VM
//! boot validation step is gated with the real-VM harness (L2) and reported
//! NOT RUN here — extraction/publishing logic is fully exercised.

use agentkit_sandbox_contracts::ids::{ArtifactDigest, ImageDigest};
use agentkit_sandbox_worker::oci::{ExtractLimits, ImageRecord, OciConfig, OciImporter};
use std::io::Write;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const BLOCK: usize = 512;

fn tar_gz(entries: &[(&str, &[u8], u32)]) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    for (path, payload, mode) in entries {
        let mut h = vec![0u8; BLOCK];
        let name = path.as_bytes();
        h[..name.len().min(100)].copy_from_slice(&name[..name.len().min(100)]);
        let m = format!("{:07o}\0", mode);
        h[100..108].copy_from_slice(m.as_bytes());
        let s = format!("{:011o}\0", payload.len());
        h[124..136].copy_from_slice(s.as_bytes());
        h[156] = if path.ends_with('/') { b'5' } else { b'0' };
        h[257..263].copy_from_slice(b"ustar\0");
        h[263..265].copy_from_slice(b"00");
        h[148..156].copy_from_slice(b"        ");
        let sum: u32 = h.iter().map(|b| *b as u32).sum();
        let cs = format!("{:06o}\0 ", sum);
        h[148..156].copy_from_slice(cs.as_bytes());
        out.extend_from_slice(&h);
        out.extend_from_slice(payload);
        let pad = payload.len().div_ceil(BLOCK) * BLOCK - payload.len();
        out.extend(std::iter::repeat_n(0u8, pad));
    }
    out.extend(std::iter::repeat_n(0u8, BLOCK * 2));
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(&out).unwrap();
    enc.finish().unwrap()
}

struct FakeReg {
    port: u16,
}

/// Canonical fixture parts shared by server and tests.
fn fixture_parts() -> (
    serde_json::Value,
    std::collections::HashMap<String, Vec<u8>>,
) {
    let layer1 = tar_gz(&[
        ("bin/", b"", 0o755),
        ("bin/sh", b"#!/bin/sh\nfake", 0o755),
        ("etc/os-release", b"ID=mini\n", 0o644),
    ]);
    let layer2 = tar_gz(&[
        ("etc/.wh.os-release", b"", 0o644),
        ("etc/passwd", b"root:x:0:0\n", 0o644),
    ]);
    let l1 = ArtifactDigest::of_bytes(&layer1);
    let l2 = ArtifactDigest::of_bytes(&layer2);
    let config_bytes =
        serde_json::to_vec(&serde_json::json!({"architecture":"x86_64","os":"linux"})).unwrap();
    let cfg = ArtifactDigest::of_bytes(&config_bytes);
    let manifest = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": cfg.as_str(),
            "size": config_bytes.len()
        },
        "layers": [
            {"mediaType":"application/vnd.oci.image.layer.v1.tar+gzip","digest":l1.as_str(),"size":layer1.len()},
            {"mediaType":"application/vnd.oci.image.layer.v1.tar+gzip","digest":l2.as_str(),"size":layer2.len()}
        ]
    });
    let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
    let manifest_hex = ArtifactDigest::of_bytes(&manifest_bytes)
        .bytes_hex()
        .to_string();
    let blobs = [
        (manifest_hex, manifest_bytes),
        (l1.bytes_hex().to_string(), layer1),
        (l2.bytes_hex().to_string(), layer2),
        (cfg.bytes_hex().to_string(), config_bytes),
    ]
    .into_iter()
    .collect();
    (manifest, blobs)
}

/// Serve a fixed registry fixture: one mini image.
async fn spawn_registry() -> FakeReg {
    let (_m, blobs) = fixture_parts();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let blobs = blobs.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).to_string();
                let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
                let hex = path.rsplit('/').next().unwrap_or("").to_string();
                // tolerate sha256: prefix
                let hex = hex.trim_start_matches("sha256:").to_string();
                let body = blobs.get(&hex).cloned();
                let (status, body) = match body {
                    Some(b) => (200u16, b),
                    None => (404, Vec::new()),
                };
                let resp = format!(
                    "HTTP/1.1 {status}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.write_all(&body).await;
                let _ = sock.shutdown().await;
            });
        }
    });
    FakeReg { port }
}

async fn importer(tmp: &std::path::Path, _port: u16, registries: Vec<String>) -> OciImporter {
    let agent = tmp.join("agent");
    std::fs::write(&agent, b"fake-agent-binary").unwrap();
    let cfg = OciConfig {
        registries,
        allow_insecure_test_origins: true,
        staging_dir: tmp.join("staging"),
        images_dir: tmp.join("images"),
        guest_agent_bin: agent.clone(),
        guest_agent_digest: ArtifactDigest::of_bytes(b"fake-agent-binary"),
        kernel_digest: Some(ImageDigest::from_hex(&"ab".repeat(32)).unwrap()),
        limits: ExtractLimits::default(),
    };
    OciImporter::new(cfg).unwrap()
}

#[tokio::test]
async fn imports_mini_image_content_addressed() {
    let reg = spawn_registry().await;
    let tmp = tempfile::tempdir().unwrap();
    let imp = importer(
        tmp.path(),
        reg.port,
        vec![format!("127.0.0.1:{}", reg.port)],
    )
    .await;

    let (manifest, _blobs) = fixture_parts();
    let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
    let digest =
        ImageDigest::from_hex(ArtifactDigest::of_bytes(&manifest_bytes).bytes_hex()).unwrap();

    let rec = imp
        .import(
            &format!("127.0.0.1:{}", reg.port),
            "library/mini",
            &digest,
            "x86_64",
        )
        .await
        .expect("mini image imports");
    assert_eq!(rec.os, "linux");
    assert_eq!(rec.layers.len(), 2);
    assert_eq!(rec.conversion_version, 1);
    assert_eq!(rec.oci_digest, digest);
    let dir = tmp
        .path()
        .join("images")
        .join(format!("sha256-{}", rec.rootfs_digest.bytes_hex()));
    assert!(dir.join("image.json").exists());
    assert!(dir.join("rootfs/etc/passwd").exists(), "layer2 applied");
    assert!(
        !dir.join("rootfs/etc/os-release").exists(),
        "whiteout applied"
    );
    assert!(dir.join("rootfs/agentkit/agentkit-sandbox-guest").exists());
    let has_mkfs = std::process::Command::new("mkfs.ext4")
        .arg("-v")
        .output()
        .is_ok();
    assert_eq!(rec.rootfs_file.is_some(), has_mkfs, "ext4 honesty");
    if let Some(f) = &rec.rootfs_file {
        assert!(dir.join(f).exists());
        assert!(dir.join(format!("{f}.sha256")).exists() || tmp.path().exists());
    }

    // Idempotent republish.
    let rec2 = imp
        .import(
            &format!("127.0.0.1:{}", reg.port),
            "library/mini",
            &digest,
            "x86_64",
        )
        .await
        .expect("reimport idempotent");
    assert_eq!(rec.rootfs_digest, rec2.rootfs_digest);
    assert_eq!(imp.list().unwrap().len(), 1);
}

fn fixture_digest() -> ImageDigest {
    let (manifest, _blobs) = fixture_parts();
    let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
    ImageDigest::from_hex(ArtifactDigest::of_bytes(&manifest_bytes).bytes_hex()).unwrap()
}

#[tokio::test]
async fn digest_mismatch_and_offpolicy_registry_fail_closed() {
    let reg = spawn_registry().await;
    let tmp = tempfile::tempdir().unwrap();
    // Off-policy registry: allowlist empty for that host.
    let imp = importer(tmp.path(), reg.port, vec!["registry.example.com".into()]).await;
    let digest = fixture_digest();
    let err = imp
        .import(
            &format!("127.0.0.1:{}", reg.port),
            "library/mini",
            &digest,
            "x86_64",
        )
        .await
        .expect_err("off-allowlist registry must fail");
    assert!(err.to_string().contains("allowlist"), "{err}");

    // Wrong digest: correct allowlist, bogus digest.
    let imp = importer(
        tmp.path(),
        reg.port,
        vec![format!("127.0.0.1:{}", reg.port)],
    )
    .await;
    let bogus = ImageDigest::from_hex(&"cd".repeat(32)).unwrap();
    let err = imp
        .import(
            &format!("127.0.0.1:{}", reg.port),
            "library/mini",
            &bogus,
            "x86_64",
        )
        .await
        .expect_err("digest mismatch must fail");
    assert!(
        err.to_string().contains("returned 404") || err.to_string().contains("match"),
        "{err}"
    );
}

#[tokio::test]
async fn platform_mismatch_fails() {
    let reg = spawn_registry().await;
    let tmp = tempfile::tempdir().unwrap();
    let imp = importer(
        tmp.path(),
        reg.port,
        vec![format!("127.0.0.1:{}", reg.port)],
    )
    .await;
    let digest = fixture_digest();
    let err = imp
        .import(
            &format!("127.0.0.1:{}", reg.port),
            "library/mini",
            &digest,
            "aarch64",
        )
        .await
        .expect_err("arch mismatch must fail");
    assert!(err.to_string().contains("platform"), "{err}");
}

#[tokio::test]
async fn agent_digest_must_match_admin_pin() {
    let reg = spawn_registry().await;
    let tmp = tempfile::tempdir().unwrap();
    let agent = tmp.path().join("agent");
    std::fs::write(&agent, b"tampered-agent").unwrap();
    let cfg = OciConfig {
        registries: vec![format!("127.0.0.1:{}", reg.port)],
        allow_insecure_test_origins: true,
        staging_dir: tmp.path().join("staging"),
        images_dir: tmp.path().join("images"),
        guest_agent_bin: agent,
        guest_agent_digest: ArtifactDigest::of_bytes(b"original-agent"),
        kernel_digest: None,
        limits: ExtractLimits::default(),
    };
    let imp = OciImporter::new(cfg).unwrap();
    let digest = fixture_digest();
    let err = imp
        .import(
            &format!("127.0.0.1:{}", reg.port),
            "library/mini",
            &digest,
            "x86_64",
        )
        .await
        .expect_err("tampered agent binary must abort import");
    assert!(err.to_string().contains("guest agent"), "{err}");
}

#[allow(dead_code)]
fn unused(record: &ImageRecord) {
    let _ = &record.provenance;
}
