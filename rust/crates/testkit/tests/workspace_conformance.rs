//! The workspaces context's in-memory twin must satisfy the repository port
//! exactly as the SQLite adapter does: one suite, shared by both
//! implementations ([`agentkit_testkit::conformance::workspace_repository`]).

use agentkit_testkit::{InMemoryWorkspaceRepository, conformance::workspace_repository};

#[tokio::test]
async fn in_memory_workspace_repository_conforms() {
    workspace_repository::run(|| async { InMemoryWorkspaceRepository::new() }).await;
}

#[test]
fn suite_has_unique_case_names() {
    let mut names = workspace_repository::CASES.to_vec();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), workspace_repository::CASES.len());
    assert!(names.len() >= 30, "{} cases", names.len());
}
