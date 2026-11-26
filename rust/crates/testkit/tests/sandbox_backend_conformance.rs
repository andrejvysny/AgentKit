//! The fake backend must satisfy the backend suite every real backend runs
//! ([`agentkit_testkit::conformance::sandbox_backend`]).

use agentkit_testkit::FakeSandboxBackend;
use agentkit_testkit::conformance::sandbox_backend::{self, fake_harness};

#[tokio::test]
async fn fake_sandbox_backend_conforms() {
    sandbox_backend::run(|| async { fake_harness(FakeSandboxBackend::new()) }).await;
}

/// The same suite on a fake that can verify absence: a stop of a
/// generation it never started answers `Stopped`, so the never-started half
/// of `proven_generations_never_start` runs too.
#[tokio::test]
async fn fake_sandbox_backend_proving_absence_conforms() {
    sandbox_backend::run(|| async {
        let fake = FakeSandboxBackend::new();
        fake.prove_absence(true);
        fake_harness(fake)
    })
    .await;
}

#[test]
fn suite_has_unique_case_names() {
    let mut names = sandbox_backend::CASES.to_vec();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), sandbox_backend::CASES.len());
    assert!(names.len() >= 25, "{} cases", names.len());
}
