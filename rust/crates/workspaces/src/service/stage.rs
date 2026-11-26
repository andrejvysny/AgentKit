//! Before a row is written: resolving a call against the binding and the
//! machine, the soft quota and the network policy. Nothing here dispatches.

use crate::error::SandboxError;
use crate::ids::Sha256;
use crate::lease::LeaseToken;
use crate::limits::{EXEC_TIMEOUT_DEFAULT_MS, EXEC_TIMEOUT_MAX_MS, OutputLimit};
use crate::machine::{MachineHandle, ReadFile};
use crate::operation::{ByteSource, FileMode, OperationKind};
use crate::path::{PathRoot, RelPath};
use crate::workspace::Workspace;

use super::WorkspaceService;
use super::lease::Renewal;
use super::paths;
use super::run::{Plan, Staged};
use super::types::{FileRef, OpContext};

/// Most numbered names a copy-in tries before it gives up.
const COPY_IN_NAMES: u32 = 100;

fn write_kind(file: &FileRef, bytes: &[u8], mode: FileMode) -> OperationKind {
    OperationKind::WriteFile {
        root: file.root,
        path: file.path.clone(),
        sha256: Sha256::of(bytes),
        bytes: ByteSource::Inline(bytes.to_vec()),
        mode,
    }
}

/// What the guest may keep of a command's log: the caller's limit, never
/// more bytes than [`OutputLimit::LOG`].
fn log_limit(requested: OutputLimit) -> OutputLimit {
    OutputLimit {
        max_bytes: requested.max_bytes.min(OutputLimit::LOG.max_bytes),
        max_lines: requested.max_lines,
    }
}

impl WorkspaceService {
    /// Resolve a plan against the binding and the machine.
    pub(super) async fn stage(
        &self,
        ctx: &OpContext,
        folder: Option<&RelPath>,
        plan: Plan,
        handle: &MachineHandle,
        lease: &LeaseToken,
    ) -> Result<Staged, SandboxError> {
        let (file, bytes, mode) = match plan {
            Plan::Exec(request) => {
                let cwd = paths::working_dir(folder, request.directory.as_ref())?;
                let timeout = request.timeout_ms.unwrap_or(EXEC_TIMEOUT_DEFAULT_MS);
                return Ok(Staged {
                    summary: request.command.clone(),
                    kind: OperationKind::Exec {
                        command: request.command,
                        cwd,
                        timeout_ms: timeout.clamp(1, EXEC_TIMEOUT_MAX_MS),
                        output_limit: log_limit(request.output_limit),
                    },
                    log_path: Some(paths::log_path(&ctx.operation_id)),
                    written: None,
                });
            }
            Plan::Write(request) => (
                paths::resolve(folder, &request.file)?,
                request.bytes,
                request.mode,
            ),
            Plan::CopyIn { name, bytes } => (
                self.free_input(folder, &name, handle, lease).await?,
                bytes,
                FileMode::REGULAR,
            ),
        };
        Ok(Staged {
            kind: write_kind(&file, &bytes, mode),
            summary: paths::display(&file),
            log_path: None,
            written: Some(file),
        })
    }

    /// The first free `<folder>/input/<name>` (then `name-1`, `name-2`, ..),
    /// after at most [`COPY_IN_NAMES`] probes. Under the lease no other
    /// writer can take it before the write; the lease is renewed whenever
    /// it is due. A probe that finds the name taken answers the file (or
    /// says it is too large or not a regular file); any other error ends
    /// the search.
    async fn free_input(
        &self,
        folder: Option<&RelPath>,
        name: &RelPath,
        handle: &MachineHandle,
        lease: &LeaseToken,
    ) -> Result<FileRef, SandboxError> {
        let input = paths::working_dir(folder, Some(&RelPath::parse("input").expect("valid")))?;
        let mut lease = lease.clone();
        for n in 0..COPY_IN_NAMES {
            if self.lease_due(&lease) {
                lease = match self.refresh(&lease).await {
                    Renewal::Kept(lease) => lease,
                    Renewal::Lost => return Err(super::lease::lost(&lease.workspace_id)),
                    Renewal::Failed(reason) => return Err(SandboxError::Unavailable { reason }),
                };
            }
            let candidate = input.join(&paths::numbered(name, n)?).map_err(|_| {
                SandboxError::PathOutsideWorkspace {
                    path: name.to_string(),
                }
            })?;
            let probe = ReadFile {
                root: PathRoot::Workspace,
                path: candidate.clone(),
                offset: 0,
                limit_bytes: 0,
                max_file_bytes: u64::MAX,
                raw: false,
            };
            match self
                .bounded("read_file", self.backend.read_file(handle, &probe))
                .await
            {
                Err(SandboxError::NotFound { .. }) => return Ok(FileRef::workspace(candidate)),
                Ok(_)
                | Err(SandboxError::FileTooLarge { .. })
                | Err(SandboxError::NotARegularFile { .. }) => {}
                Err(SandboxError::Backend { reason }) => {
                    return Err(SandboxError::Unavailable { reason });
                }
                Err(error) => return Err(error),
            }
        }
        Err(SandboxError::Unavailable {
            reason: format!("no free name for {name} in {input} after {COPY_IN_NAMES} tries"),
        })
    }

    /// The soft quota (spec "Limits"): refuse when the volume is full or
    /// the new bytes would pass the quota.
    pub(super) async fn check_quota(
        &self,
        workspace: &Workspace,
        new_bytes: u64,
    ) -> Result<(), SandboxError> {
        let call = self.backend.storage_usage(&workspace.id);
        let usage = match self.bounded("storage_usage", call).await {
            Ok(usage) => usage,
            Err(SandboxError::Backend { reason }) => {
                return Err(SandboxError::Unavailable { reason });
            }
            Err(error) => return Err(error),
        };
        let quota = usage.quota_bytes.min(self.config.quota_bytes());
        if usage.used_bytes >= quota || usage.used_bytes.saturating_add(new_bytes) > quota {
            return Err(SandboxError::QuotaExceeded {
                used_bytes: usage.used_bytes,
                quota_bytes: quota,
            });
        }
        Ok(())
    }

    /// The workspace's grants as the machine's policy, before every
    /// operation (a revoke that could not reach the machine live lands
    /// here at the latest). Serialized with live pushes, see
    /// [`WorkspaceService::push_policy`].
    pub(super) async fn apply_policy(
        &self,
        workspace: &Workspace,
        handle: &MachineHandle,
    ) -> Result<(), SandboxError> {
        match self.push_policy(&workspace.id, handle).await {
            Ok(()) => Ok(()),
            Err(SandboxError::Backend { reason }) => Err(SandboxError::Unavailable { reason }),
            Err(error) => Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_log_limit_never_passes_the_log_cap() {
        let huge = OutputLimit {
            max_bytes: u64::MAX,
            max_lines: Some(10),
        };
        assert_eq!(
            log_limit(huge),
            OutputLimit {
                max_bytes: OutputLimit::LOG.max_bytes,
                max_lines: Some(10)
            }
        );
        let small = OutputLimit {
            max_bytes: 1024,
            max_lines: None,
        };
        assert_eq!(log_limit(small), small);
    }
}
