//! The in-memory twin must satisfy the sessions port exactly as the SQLite
//! adapter does: one suite, both implementations
//! ([`agentkit_testkit::conformance::session_repository`]).

use agentkit_testkit::{InMemorySessionFixture, conformance::session_repository};

#[tokio::test]
async fn in_memory_session_repository_conforms() {
    session_repository::run(|| async { InMemorySessionFixture::new() }).await;
}

#[test]
fn suite_has_unique_case_names() {
    let mut names = session_repository::CASES.to_vec();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), session_repository::CASES.len());
    assert!(names.len() >= 25);
}
