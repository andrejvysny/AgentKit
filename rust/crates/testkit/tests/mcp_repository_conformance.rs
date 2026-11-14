//! The in-memory twin must satisfy the MCP configuration port exactly as
//! the SQLite adapter does: one suite, both implementations
//! ([`agentkit_testkit::conformance::mcp_repository`]).

use agentkit_testkit::{InMemoryMcpConfigFixture, conformance::mcp_repository};

#[tokio::test]
async fn in_memory_mcp_repository_conforms() {
    mcp_repository::run(|| async { InMemoryMcpConfigFixture::new() }).await;
}

#[test]
fn suite_has_unique_case_names() {
    let mut names = mcp_repository::CASES.to_vec();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), mcp_repository::CASES.len());
    assert!(names.len() >= 12);
}
