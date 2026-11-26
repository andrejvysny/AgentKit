use agentkit_sandbox_proto::guest::{GuestError, GuestErrorCode};
use agentkit_sandbox_proto::launcher::{LauncherError, LauncherErrorCode};
use agentkit_workspaces::{
    EXPORT_FILE_MAX_BYTES, Generation, HostPort, NetworkPolicy, PathRoot, RelPath, ResourceLimits,
    SandboxError,
};

use super::*;

fn guest_error(code: GuestErrorCode) -> GuestError {
    GuestError {
        code,
        message: "guest text".to_owned(),
    }
}

fn refused(code: LauncherErrorCode) -> LauncherFailure {
    LauncherFailure::Refused(LauncherError {
        code,
        message: "detail".to_owned(),
    })
}

#[test]
fn guest_file_errors_map_to_port_errors() {
    let path = RelPath::parse("dir/a.txt").unwrap();
    let map = |code| file_error(&guest_error(code), PathRoot::Workspace, &path, 4);
    let shown = "/workspace/dir/a.txt".to_owned();
    assert_eq!(
        map(GuestErrorCode::NotFound),
        SandboxError::NotFound {
            what: shown.clone()
        }
    );
    assert_eq!(
        map(GuestErrorCode::NotADirectory),
        SandboxError::NotFound {
            what: shown.clone()
        }
    );
    assert_eq!(
        map(GuestErrorCode::NotARegularFile),
        SandboxError::NotARegularFile {
            path: shown.clone()
        }
    );
    assert_eq!(
        map(GuestErrorCode::FileTooLarge { size: 11 }),
        SandboxError::FileTooLarge { size: 11, limit: 4 }
    );
    assert_eq!(
        map(GuestErrorCode::InvalidPath),
        SandboxError::PathOutsideWorkspace { path: shown }
    );
    for code in [
        GuestErrorCode::NoSpace,
        GuestErrorCode::Busy,
        GuestErrorCode::Internal,
        GuestErrorCode::InvalidRequest,
    ] {
        let mapped = map(code);
        assert_eq!(mapped.code(), "sandbox_backend_failed");
        assert!(!mapped.to_string().contains("guest text"), "{mapped}");
    }
}

#[test]
fn a_huge_read_limit_reports_the_guest_bound() {
    let path = RelPath::root();
    let error = guest_error(GuestErrorCode::FileTooLarge { size: 1 << 40 });
    let mapped = file_error(&error, PathRoot::Home, &path, u64::MAX);
    assert_eq!(
        mapped,
        SandboxError::FileTooLarge {
            size: 1 << 40,
            limit: EXPORT_FILE_MAX_BYTES
        }
    );
}

#[test]
fn launcher_refusals_map_to_port_errors() {
    let three = Some(Generation(3));
    let newer = launcher(
        refused(LauncherErrorCode::StaleGeneration { current: 4 }),
        three,
    );
    assert_eq!(
        newer,
        SandboxError::StaleGeneration {
            requested: Generation(3),
            current: Generation(4)
        }
    );
    let same = launcher(
        refused(LauncherErrorCode::StaleGeneration { current: 3 }),
        three,
    );
    assert!(matches!(same, SandboxError::StaleGeneration { .. }));
    let older = launcher(
        refused(LauncherErrorCode::StaleGeneration { current: 2 }),
        three,
    );
    assert_eq!(older.code(), "sandbox_backend_failed", "{older}");
    let used_up = launcher(refused(LauncherErrorCode::Busy), three);
    assert_eq!(
        used_up,
        SandboxError::StaleGeneration {
            requested: Generation(3),
            current: Generation(3)
        }
    );
    let busy = launcher(refused(LauncherErrorCode::Busy), None);
    assert_eq!(busy.code(), "sandbox_backend_failed", "{busy}");
    let cases = [
        (LauncherErrorCode::NotFound, "sandbox_not_found"),
        (LauncherErrorCode::InvalidRequest, "sandbox_backend_failed"),
        (LauncherErrorCode::RuntimeFailed, "sandbox_unavailable"),
        (LauncherErrorCode::ImageMismatch, "sandbox_unavailable"),
    ];
    for (code, expected) in cases {
        assert_eq!(launcher(refused(code), three).code(), expected, "{code:?}");
    }
    let down = launcher(LauncherFailure::Unreachable("refused".into()), None);
    assert_eq!(down.code(), "sandbox_unavailable");
}

#[test]
fn limits_become_a_tier_and_grants_targets() {
    let tier = tier(&ResourceLimits::DEFAULT);
    assert_eq!(
        (tier.cpus_milli, tier.memory_bytes, tier.pids),
        (2000, 2048 * 1024 * 1024, 512)
    );
    let policy = NetworkPolicy {
        grants: vec![
            HostPort::new("pypi.org", 443).unwrap(),
            HostPort::new("10.0.0.1", 80).unwrap(),
        ],
    };
    let targets = targets(&policy).unwrap();
    let shown: Vec<String> = targets.iter().map(ToString::to_string).collect();
    assert_eq!(shown, ["pypi.org:443", "10.0.0.1:80"]);
}
