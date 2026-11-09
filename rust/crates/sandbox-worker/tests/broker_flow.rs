//! Broker e2e (L1, §9): guest-initiated structured fetches over the fake
//! guest link, served by an in-process fake registry. Exercises the real
//! broker: structured validation, frozen-origin policy, per-hop redirect
//! revalidation, digest binding, budgets, and honest denial. These are
//! contract tests — NOT a qualification of network isolation (that is L2).

mod support;

use agentkit_sandbox_contracts::api::PackageFetchRequest;
use agentkit_sandbox_contracts::api::messages::{HostToWorker, WorkerToGuest};
use agentkit_sandbox_contracts::ids::ArtifactDigest;
use agentkit_sandbox_contracts::policy::{RegistryEndpoint, RegistryPolicy};
use agentkit_sandbox_contracts::spec::Ecosystem;
use agentkit_sandbox_worker::broker::{Broker, BrokerConfig};
use agentkit_sandbox_worker::guestlink::SimBehavior;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use support::Harness;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

const WHEEL: &[u8] = b"fake-wheel-content-v1";
const CRATE_BYTES: &[u8] = b"fake-crate-tar-gz-bytes";
const NPM_TGZ: &[u8] = b"fake-npm-tarball-bytes";

fn sha(b: &[u8]) -> String {
    ArtifactDigest::of_bytes(b).bytes_hex().to_string()
}

/// Minimal HTTP/1.1 responder; routes fixed paths, everything else 404.
/// Counts every request (for the "no egress when disabled" assertion).
async fn spawn_fake_registry() -> (u16, Arc<AtomicU32>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let counter = Arc::new(AtomicU32::new(0));
    let c = counter.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let c = c.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).to_string();
                let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
                c.fetch_add(1, Ordering::SeqCst);
                let (status, body, loc) = route(&path, port);
                let mut resp = format!("HTTP/1.1 {status}\r\ncontent-length: {}\r\n", body.len());
                if let Some(l) = loc {
                    resp.push_str(&format!("location: {l}\r\n"));
                }
                resp.push_str("connection: close\r\n\r\n");
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.write_all(&body).await;
                let _ = sock.shutdown().await;
            });
        }
    });
    (port, counter)
}

fn route(path: &str, port: u16) -> (u16, Vec<u8>, Option<String>) {
    let ok = |b: &[u8]| (200u16, b.to_vec(), None);
    match path {
        // pypi: pinned/unpinned happy paths share one doc.
        "/pypi/requests/2.32.3/json" => (
            200,
            format!(
                r#"{{"urls":[{{"filename":"requests-2.32.3-py3-none-any.whl","url":"http://127.0.0.1:{port}/files/requests-2.32.3.whl","digests":{{"sha256":"{}"}}}}]}}"#,
                sha(WHEEL)
            )
            .into_bytes(),
            None,
        ),
        "/files/requests-2.32.3.whl" => ok(WHEEL),
        // pypi doc whose artifact lives on a non-permitted origin.
        "/pypi/evil/1.0.0/json" => (
            200,
            format!(
                r#"{{"urls":[{{"filename":"evil.whl","url":"http://evil.example.invalid/x.whl","digests":{{"sha256":"{}"}}}}]}}"#,
                sha(WHEEL)
            )
            .into_bytes(),
            None,
        ),
        // redirect chain inside the permitted test origin (crates pattern).
        "/api/v1/crates/mycrate/0.1.0/download" => (
            302,
            Vec::new(),
            Some(format!("http://127.0.0.1:{port}/dl-stage1")),
        ),
        "/dl-stage1" => (
            302,
            Vec::new(),
            Some(format!("http://127.0.0.1:{port}/dl/mycrate-0.1.0.crate")),
        ),
        "/dl/mycrate-0.1.0.crate" => ok(CRATE_BYTES),
        // redirect chain that LEAVES the permitted origin.
        "/api/v1/crates/escaped/0.1.0/download" => (
            302,
            Vec::new(),
            Some(format!("http://127.0.0.1:{port}/hop-out")),
        ),
        "/hop-out" => (
            302,
            Vec::new(),
            Some("http://evil.example.invalid:9/x.crate".into()),
        ),
        // npm: with and without integrity claim.
        "/@scope%2Fpkg/1.2.3" => (
            200,
            format!(
                r#"{{"dist":{{"tarball":"http://127.0.0.1:{port}/scope-pkg-1.2.3.tgz","integrity":"sha512-{}"}}}}"#,
                npm_integrity()
            )
            .into_bytes(),
            None,
        ),
        "/no-integrity/1.0.0" => (
            200,
            format!(
                r#"{{"dist":{{"tarball":"http://127.0.0.1:{port}/scope-pkg-1.2.3.tgz"}}}}"#
            )
            .into_bytes(),
            None,
        ),
        "/scope-pkg-1.2.3.tgz" => ok(NPM_TGZ),
        _ => (404, Vec::new(), None),
    }
}

fn npm_integrity() -> String {
    use base64::Engine;
    use sha2::Digest;
    base64::engine::general_purpose::STANDARD.encode(sha2::Sha512::digest(NPM_TGZ))
}

fn test_policy(requests_per_window: u32) -> RegistryPolicy {
    let ep = |eco| RegistryEndpoint {
        ecosystem: eco,
        host: "127.0.0.1".into(),
        max_artifact_bytes: 4 * 1024 * 1024,
        max_redirects: 4,
    };
    RegistryPolicy {
        id: agentkit_sandbox_contracts::ids::RegistryPolicyId::new("test-v1").unwrap(),
        endpoints: vec![
            ep(Ecosystem::Pypi),
            ep(Ecosystem::Npm),
            ep(Ecosystem::Crates),
        ],
        requests_per_window,
        egress_bytes_per_window: 8 * 1024 * 1024,
        window_sec: 3600,
        fetch_deadline_sec: 15,
    }
}

struct Setup {
    _h: Harness,
    rx: mpsc::UnboundedReceiver<WorkerToGuest>,
    requests: Arc<AtomicU32>,
}

async fn setup(
    fetches: Vec<PackageFetchRequest>,
    policy: Option<RegistryPolicy>,
    broker_enabled: bool,
    requests_per_window: u32,
) -> Setup {
    let (port, requests) = spawn_fake_registry().await;
    let h = Harness::new();
    if broker_enabled {
        let dir = std::env::temp_dir().join(format!(
            "ak-broker-cache-{}-{}",
            std::process::id(),
            requests_per_window
        ));
        let b = Broker::new(BrokerConfig {
            enabled: true,
            test_origins: vec![format!("127.0.0.1:{port}")],
            cache_dir: dir,
        })
        .unwrap();
        h.svc.attach_broker(Arc::new(b));
    }
    let (tx, rx) = mpsc::unbounded_channel();
    h.set_sim_behavior(Some(SimBehavior {
        broker_requests: fetches,
        capture: Some(tx),
        ..Default::default()
    }));
    h.create_ready("alice", "sbx").await;
    if let Some(p) = policy {
        let body = h
            .ok(
                "bpol-1",
                9,
                &HostToWorker::BrokerPolicyPut {
                    policy: p,
                    generation: agentkit_sandbox_contracts::ids::PolicyGeneration(1),
                },
            )
            .await;
        assert!(matches!(
            body,
            agentkit_sandbox_contracts::api::messages::WorkerReplyBody::Ack {}
        ));
    }
    Setup {
        _h: h,
        rx,
        requests,
    }
}

async fn await_broker_frames(
    rx: &mut mpsc::UnboundedReceiver<WorkerToGuest>,
    want_final: bool,
) -> Vec<WorkerToGuest> {
    let mut seen = Vec::new();
    for _ in 0..500 {
        if let Ok(msg) = rx.try_recv() {
            match msg {
                WorkerToGuest::BrokerChunk { final_chunk, .. } => {
                    let last = final_chunk;
                    seen.push(msg);
                    if !want_final || last {
                        return seen;
                    }
                    continue;
                }
                WorkerToGuest::BrokerDeny { .. } => {
                    seen.push(msg);
                    return seen;
                }
                _ => {}
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    seen
}

fn fetch(eco: Ecosystem, package: &str, version: &str, pin: Option<&str>) -> PackageFetchRequest {
    PackageFetchRequest {
        version: 1,
        ecosystem: eco,
        package: package.into(),
        version_spec: version.into(),
        artifact_digest: pin.map(|h| ArtifactDigest::from_hex(h).unwrap()),
        sandbox_generation: 1,
    }
}

#[tokio::test]
async fn pypi_pinned_digest_streams_verified_chunks() {
    let mut s = setup(
        vec![fetch(
            Ecosystem::Pypi,
            "requests",
            "2.32.3",
            Some(&sha(WHEEL)),
        )],
        Some(test_policy(100)),
        true,
        100,
    )
    .await;
    let frames = await_broker_frames(&mut s.rx, true).await;
    let mut acc = Vec::new();
    let mut saw_final = false;
    for f in frames {
        match f {
            WorkerToGuest::BrokerChunk {
                sequence,
                bytes_b64,
                chunk_digest,
                final_chunk,
                ..
            } => {
                use base64::Engine;
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(&bytes_b64)
                    .unwrap();
                // Every chunk must carry its own verified digest (S16/S17).
                assert_eq!(
                    ArtifactDigest::of_bytes(&bytes),
                    chunk_digest,
                    "chunk {sequence}"
                );
                acc.extend_from_slice(&bytes);
                saw_final |= final_chunk;
            }
            WorkerToGuest::BrokerDeny { reason, .. } => panic!("unexpected deny: {reason}"),
            _ => {}
        }
    }
    assert!(saw_final, "final chunk flag never set");
    assert_eq!(acc, WHEEL);
}

#[tokio::test]
async fn pypi_unpinned_binds_registry_declared_digest() {
    let mut s = setup(
        vec![fetch(Ecosystem::Pypi, "requests", "2.32.3", None)],
        Some(test_policy(100)),
        true,
        100,
    )
    .await;
    let frames = await_broker_frames(&mut s.rx, true).await;
    assert!(
        frames.iter().any(|f| matches!(
            f,
            WorkerToGuest::BrokerChunk {
                final_chunk: true,
                ..
            }
        )),
        "expected final chunk, got {frames:?}"
    );
}

#[tokio::test]
async fn pypi_pin_mismatch_denied() {
    let mut s = setup(
        vec![fetch(
            Ecosystem::Pypi,
            "requests",
            "2.32.3",
            Some(&sha(b"wrong")),
        )],
        Some(test_policy(100)),
        true,
        100,
    )
    .await;
    let frames = await_broker_frames(&mut s.rx, false).await;
    let WorkerToGuest::BrokerDeny { reason, .. } = &frames[0] else {
        panic!("expected deny, got {frames:?}");
    };
    assert!(reason.contains("digest"), "reason: {reason}");
}

#[tokio::test]
async fn artifact_on_off_policy_origin_denied() {
    let mut s = setup(
        vec![fetch(Ecosystem::Pypi, "evil", "1.0.0", None)],
        Some(test_policy(100)),
        true,
        100,
    )
    .await;
    let frames = await_broker_frames(&mut s.rx, false).await;
    let WorkerToGuest::BrokerDeny { reason, .. } = &frames[0] else {
        panic!("expected deny, got {frames:?}");
    };
    assert!(
        reason.contains("origin") || reason.contains("https"),
        "reason: {reason}"
    );
}

#[tokio::test]
async fn crates_two_hop_redirect_within_policy_ok() {
    let mut s = setup(
        vec![fetch(
            Ecosystem::Crates,
            "mycrate",
            "0.1.0",
            Some(&sha(CRATE_BYTES)),
        )],
        Some(test_policy(100)),
        true,
        100,
    )
    .await;
    let frames = await_broker_frames(&mut s.rx, true).await;
    assert!(
        frames.iter().any(|f| matches!(
            f,
            WorkerToGuest::BrokerChunk {
                final_chunk: true,
                ..
            }
        )),
        "expected final chunk through allowed redirects, got {frames:?}"
    );
}

#[tokio::test]
async fn redirect_leaving_policy_origin_denied() {
    let mut s = setup(
        vec![fetch(
            Ecosystem::Crates,
            "escaped",
            "0.1.0",
            Some(&sha(CRATE_BYTES)),
        )],
        Some(test_policy(100)),
        true,
        100,
    )
    .await;
    let frames = await_broker_frames(&mut s.rx, false).await;
    let WorkerToGuest::BrokerDeny { reason, .. } = &frames[0] else {
        panic!("expected deny, got {frames:?}");
    };
    assert!(
        reason.contains("origin") || reason.contains("https"),
        "reason: {reason}"
    );
}

#[tokio::test]
async fn npm_requires_verifiable_integrity() {
    let mut ok = setup(
        vec![fetch(Ecosystem::Npm, "@scope/pkg", "1.2.3", None)],
        Some(test_policy(100)),
        true,
        100,
    )
    .await;
    let frames = await_broker_frames(&mut ok.rx, true).await;
    assert!(
        frames.iter().any(|f| matches!(
            f,
            WorkerToGuest::BrokerChunk {
                final_chunk: true,
                ..
            }
        )),
        "integrity-verifiable npm artifact should flow, got {frames:?}"
    );

    let mut bad = setup(
        vec![fetch(Ecosystem::Npm, "no-integrity", "1.0.0", None)],
        Some(test_policy(100)),
        true,
        100,
    )
    .await;
    let frames = await_broker_frames(&mut bad.rx, false).await;
    let WorkerToGuest::BrokerDeny { reason, .. } = &frames[0] else {
        panic!("expected deny, got {frames:?}");
    };
    assert!(reason.contains("integrity"), "reason: {reason}");
}

#[tokio::test]
async fn tenant_request_budget_is_enforced() {
    // Two sequential fetches; policy allows only 3 requests total, each
    // pypi fetch costs 2 (metadata + artifact).
    let mut s = setup(
        vec![
            fetch(Ecosystem::Pypi, "requests", "2.32.3", Some(&sha(WHEEL))),
            fetch(Ecosystem::Pypi, "requests", "2.32.3", Some(&sha(WHEEL))),
        ],
        Some(test_policy(3)),
        true,
        3,
    )
    .await;
    let mut denies = 0;
    let mut finals = 0;
    let started = std::time::Instant::now();
    while (denies == 0 || finals == 0) && started.elapsed() < std::time::Duration::from_secs(5) {
        if let Ok(msg) = s.rx.try_recv() {
            match msg {
                WorkerToGuest::BrokerChunk {
                    final_chunk: true, ..
                } => finals += 1,
                WorkerToGuest::BrokerDeny { reason, .. } => {
                    assert!(reason.contains("budget"), "reason: {reason}");
                    denies += 1;
                }
                _ => {}
            }
        } else {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
    assert_eq!(finals, 1, "first fetch should complete");
    assert_eq!(denies, 1, "second fetch must hit the budget");
}

#[tokio::test]
async fn disabled_broker_denies_and_never_egresses() {
    let mut s = setup(
        vec![fetch(
            Ecosystem::Pypi,
            "requests",
            "2.32.3",
            Some(&sha(WHEEL)),
        )],
        Some(test_policy(100)),
        false,
        100,
    )
    .await;
    let frames = await_broker_frames(&mut s.rx, false).await;
    let WorkerToGuest::BrokerDeny { reason, .. } = &frames[0] else {
        panic!("expected deny, got {frames:?}");
    };
    assert!(reason.contains("not enabled"), "reason: {reason}");
    // Only the guest handshake happened; zero registry contacts.
    assert_eq!(
        s.requests.load(Ordering::SeqCst),
        0,
        "no egress while disabled"
    );
}

#[tokio::test]
async fn stale_generation_denied() {
    let mut f = fetch(Ecosystem::Pypi, "requests", "2.32.3", Some(&sha(WHEEL)));
    f.sandbox_generation = 42;
    let mut s = setup(vec![f], Some(test_policy(100)), true, 100).await;
    let frames = await_broker_frames(&mut s.rx, false).await;
    let WorkerToGuest::BrokerDeny { reason, .. } = &frames[0] else {
        panic!("expected deny, got {frames:?}");
    };
    assert!(reason.contains("generation"), "reason: {reason}");
}
