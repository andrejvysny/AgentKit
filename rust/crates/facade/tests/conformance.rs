//! Driver conformance for the real Claude Code backend driven through the
//! facade harness with a scripted in-process fixture locator (AGENTKIT-117).

use std::path::PathBuf;

use async_trait::async_trait;

use agentkit_claude::backend::default_backend;
use agentkit_claude::discovery::ClaudeSupportMatrix;
use agentkit_facade::conformance::{ConformanceReport, required_cases, run_driver_conformance};
use agentkit_native::{
    AuthEvidence, AuthExpectation, AuthMode, ContentPolicy, EXECUTION_TARGET_SCHEMA_VERSION,
    ExecutableEvidence, ExecutableLocator, NativeExecutionTarget, NativeProfile, NativeRefusal,
    PolicyState,
};

/// Pid-unique fixture executable under the temp dir: a real regular file so
/// preflight's replacement recheck observes exactly what the live path sees.
fn fixture_path() -> PathBuf {
    std::env::temp_dir()
        .join(format!(
            "agentkit-facade-conformance-{}",
            std::process::id()
        ))
        .join("claude")
}

fn ensure_fixture() {
    let path = fixture_path();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"official-cli-conformance-fixture").unwrap();
}

struct ScriptedLocator;

#[async_trait]
impl ExecutableLocator for ScriptedLocator {
    async fn locate(&self) -> Result<ExecutableEvidence, NativeRefusal> {
        Ok(ExecutableEvidence {
            path: fixture_path(),
            version: "2.1.310".to_owned(),
            digest: None,
        })
    }

    async fn auth_status(&self) -> Result<AuthEvidence, NativeRefusal> {
        Ok(AuthEvidence {
            mode: AuthMode::Subscription {
                account_scope: "conformance".to_owned(),
            },
            policy: PolicyState::None,
            config_directory: None,
        })
    }
}

fn plain_target() -> NativeExecutionTarget {
    NativeExecutionTarget {
        schema_version: EXECUTION_TARGET_SCHEMA_VERSION,
        backend: "claude-code".to_owned(),
        backend_version: Some("2.1.310".to_owned()),
        profile: NativeProfile::PlainChat,
        executable: fixture_path().display().to_string(),
        requested_model: None,
        requested_effort: None,
        auth_expectation: AuthExpectation::SubscriptionLogin,
        capabilities: ClaudeSupportMatrix::snapshot(Some("2.1.310")),
        config_identity: "agentkit-facade-conformance".to_owned(),
        system_prompt_revision: None,
        content: ContentPolicy::default(),
    }
}

#[tokio::test]
async fn claude_driver_passes_every_required_conformance_case() {
    ensure_fixture();
    let tmp = tempfile::tempdir().unwrap();
    let (backend, _default_locator) =
        default_backend(Some(fixture_path()), tmp.path().join("runs"));
    let cases = required_cases();
    let reports = run_driver_conformance(&backend, &ScriptedLocator, plain_target, &cases).await;

    assert_eq!(reports.len(), cases.len());
    let failures: Vec<&ConformanceReport> = reports.iter().filter(|r| !r.passed).collect();
    assert!(failures.is_empty(), "conformance failures: {failures:#?}");
    for report in &reports {
        assert!(
            !report.detail.trim().is_empty(),
            "empty detail for {}",
            report.case_id
        );
    }
}

#[test]
fn required_case_ids_are_unique_snake_case() {
    let cases = required_cases();
    assert!(!cases.is_empty());
    let mut seen = std::collections::BTreeSet::new();
    for case in &cases {
        assert!(
            case.id
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
                && !case.id.is_empty()
                && !case.id.starts_with('_')
                && !case.id.ends_with('_'),
            "case id '{}' is not snake_case",
            case.id
        );
        assert!(seen.insert(case.id), "duplicate case id '{}'", case.id);
        assert!(!case.title.trim().is_empty());
    }
}
