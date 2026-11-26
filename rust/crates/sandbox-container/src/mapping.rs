//! Wire types to port types and back, and the error tables: guest error
//! codes and launcher refusals as `SandboxError`s. Text from the guest is
//! never copied into an error; the guest shares the workload's uid.

use agentkit_egress::Target;
use agentkit_sandbox_proto::guest::{
    self, ExecRequest, GuestError, GuestErrorCode, Request, WriteRequest,
};
use agentkit_sandbox_proto::launcher::{LauncherError, LauncherErrorCode, Tier};
use agentkit_workspaces::{
    ByteSource, EXPORT_FILE_MAX_BYTES, Generation, NetworkPolicy, OperationKind, OperationRequest,
    OperationState, OutputLimit, PathRoot, RelPath, ResourceLimits, SandboxError,
};

use crate::launcher::LauncherFailure;

const MIB: u64 = 1024 * 1024;
/// Longest launcher message kept in an error.
const MESSAGE_MAX: usize = 512;

/// The port's state of a wire state.
pub(crate) fn state(wire: guest::OperationState) -> OperationState {
    match wire {
        guest::OperationState::Accepted => OperationState::Accepted,
        guest::OperationState::Running => OperationState::Running,
        guest::OperationState::Succeeded { exit } => OperationState::Succeeded { exit },
        guest::OperationState::Failed { exit, signal } => OperationState::Failed { exit, signal },
        guest::OperationState::TimedOut => OperationState::TimedOut,
        guest::OperationState::Cancelled => OperationState::Cancelled,
        guest::OperationState::OutputLimit => OperationState::OutputLimit,
        guest::OperationState::Unknown => OperationState::Unknown,
    }
}

pub(crate) fn root(root: PathRoot) -> guest::PathRoot {
    match root {
        PathRoot::Workspace => guest::PathRoot::Workspace,
        PathRoot::Home => guest::PathRoot::Home,
    }
}

fn output_limit(limit: OutputLimit) -> guest::OutputLimit {
    guest::OutputLimit {
        max_bytes: limit.max_bytes,
        max_lines: limit.max_lines,
    }
}

/// The guest request of `op`, and the bytes of a write.
pub(crate) fn dispatch_request(op: &OperationRequest) -> (Request, Option<Vec<u8>>) {
    let id = op.id.as_str().to_owned();
    let args_sha256 = op.args_sha256.as_str().to_owned();
    match &op.kind {
        OperationKind::Exec {
            command,
            cwd,
            timeout_ms,
            output_limit: limit,
        } => {
            let exec = ExecRequest {
                op: id,
                args_sha256,
                command: command.clone(),
                cwd: cwd.as_str().to_owned(),
                timeout_ms: *timeout_ms,
                output_limit: output_limit(*limit),
                tz: None,
            };
            (Request::Exec(exec), None)
        }
        OperationKind::WriteFile {
            root: tree,
            path,
            sha256,
            bytes: ByteSource::Inline(bytes),
            mode,
        } => {
            let write = WriteRequest {
                op: id,
                args_sha256,
                root: root(*tree),
                path: path.as_str().to_owned(),
                sha256: sha256.as_str().to_owned(),
                size: bytes.len() as u64,
                mode: mode.bits(),
            };
            (Request::Write(write), Some(bytes.clone()))
        }
    }
}

/// The launcher tier of the machine's limits.
pub(crate) fn tier(limits: &ResourceLimits) -> Tier {
    Tier {
        cpus_milli: limits.vcpu.saturating_mul(1000),
        memory_bytes: limits.memory_mib.saturating_mul(MIB),
        pids: limits.pids,
    }
}

/// The egress targets of a policy.
pub(crate) fn targets(policy: &NetworkPolicy) -> Result<Vec<Target>, SandboxError> {
    policy
        .grants
        .iter()
        .map(|grant| {
            Target::new(grant.host(), grant.port()).map_err(|error| SandboxError::Backend {
                reason: format!("grant {grant} cannot be applied: {error}"),
            })
        })
        .collect()
}

fn shown(tree: PathRoot, path: &RelPath) -> String {
    if path.is_root() {
        tree.guest_dir().to_owned()
    } else {
        format!("{}/{}", tree.guest_dir(), path.as_str())
    }
}

/// The name of a guest error code, for messages.
pub(crate) fn code_name(code: GuestErrorCode) -> &'static str {
    match code {
        GuestErrorCode::NotFound => "not_found",
        GuestErrorCode::NotADirectory => "not_a_directory",
        GuestErrorCode::NotARegularFile => "not_a_regular_file",
        GuestErrorCode::FileTooLarge { .. } => "file_too_large",
        GuestErrorCode::DigestMismatch => "digest_mismatch",
        GuestErrorCode::SizeMismatch => "size_mismatch",
        GuestErrorCode::InvalidPath => "invalid_path",
        GuestErrorCode::InvalidRequest => "invalid_request",
        GuestErrorCode::ProtocolMismatch => "protocol_mismatch",
        GuestErrorCode::GenerationMismatch => "generation_mismatch",
        GuestErrorCode::NoSpace => "no_space",
        GuestErrorCode::Busy => "busy",
        GuestErrorCode::Internal => "internal",
    }
}

/// A guest refusal of a request that names no file.
pub(crate) fn guest_failure(what: &str, error: &GuestError) -> SandboxError {
    SandboxError::Backend {
        reason: format!("the guest refused {what}: {}", code_name(error.code)),
    }
}

/// A guest refusal of a read or a listing of `path` below `tree`. The
/// guest reads at most [`EXPORT_FILE_MAX_BYTES`] of any file, so that is
/// the limit a larger request reports.
pub(crate) fn file_error(
    error: &GuestError,
    tree: PathRoot,
    path: &RelPath,
    max_file_bytes: u64,
) -> SandboxError {
    let path = shown(tree, path);
    match error.code {
        GuestErrorCode::NotFound | GuestErrorCode::NotADirectory => {
            SandboxError::NotFound { what: path }
        }
        GuestErrorCode::NotARegularFile => SandboxError::NotARegularFile { path },
        GuestErrorCode::FileTooLarge { size } => SandboxError::FileTooLarge {
            size,
            limit: max_file_bytes.min(EXPORT_FILE_MAX_BYTES),
        },
        GuestErrorCode::InvalidPath => SandboxError::PathOutsideWorkspace { path },
        _ => guest_failure("the file request", error),
    }
}

fn message(error: &LauncherError) -> String {
    error.message.chars().take(MESSAGE_MAX).collect()
}

/// A launcher failure as a port error. `requested` is the generation an
/// `ensure` asked for: a refusal naming a newer current generation is
/// `StaleGeneration`; one naming an older generation means that one still
/// runs, which the caller must stop first.
///
/// `busy` at an `ensure` means the requested generation cannot start: it
/// had its one container already (exited, or gone after a failed start
/// whose stop was not proven), or another generation's container exists.
/// It is `StaleGeneration` at the requested generation, so the service
/// proves that generation stopped and moves past it instead of retrying
/// it for ever; outside an `ensure` it stays a backend failure.
pub(crate) fn launcher(failure: LauncherFailure, requested: Option<Generation>) -> SandboxError {
    let error = match failure {
        LauncherFailure::Unreachable(reason) => {
            return SandboxError::Unavailable {
                reason: format!("the sandbox launcher is unreachable: {reason}"),
            };
        }
        LauncherFailure::Refused(error) => error,
    };
    let text = message(&error);
    match error.code {
        LauncherErrorCode::StaleGeneration { current } => match requested {
            Some(requested) if Generation(current) >= requested => SandboxError::StaleGeneration {
                requested,
                current: Generation(current),
            },
            _ => SandboxError::Backend {
                reason: format!("generation {current} of the workspace still runs; stop it first"),
            },
        },
        LauncherErrorCode::Busy => match requested {
            Some(requested) => SandboxError::StaleGeneration {
                requested,
                current: requested,
            },
            None => SandboxError::Backend {
                reason: format!("a machine of the workspace still runs; stop it first ({text})"),
            },
        },
        LauncherErrorCode::NotFound => SandboxError::NotFound {
            what: format!("launcher record ({text})"),
        },
        LauncherErrorCode::InvalidRequest => SandboxError::Backend {
            reason: format!("the launcher refused the request as invalid ({text})"),
        },
        LauncherErrorCode::RuntimeFailed => SandboxError::Unavailable {
            reason: format!("the container runtime failed ({text})"),
        },
        LauncherErrorCode::ImageMismatch => SandboxError::Unavailable {
            reason: format!("the sandbox image is not installed on this server ({text})"),
        },
    }
}

#[cfg(test)]
mod tests;
