//! The coding tool pack as a host `ToolContributor` (namespace `coding`).
//! Port of `packages/toolpacks-coding/src/pack.ts`.
//!
//! It offers every tool on every run; whether a call may proceed is the
//! `CodingPermissionGuard`'s decision, not the contributor's.

use std::sync::Arc;

use agentkit_core::CancellationToken;
use agentkit_host::{ContributedTool, ToolContributionContext, ToolContributor};
use async_trait::async_trait;

use crate::context::PackEnv;
use crate::coordination::default_workspace_coordinator;
use crate::errors::CodingError;
use crate::exec_tools::{run_command_tool, run_tests_tool};
use crate::fs_tools::{edit_file_tool, list_dir_tool, read_file_tool, write_file_tool};
use crate::git_tool::git_inspect_tool;
use crate::guard::CODING_NAMESPACE;
use crate::limits::CodingLimits;
use crate::plan_tool::submit_plan_tool;
use crate::process::startup_recovery;
use crate::sandbox::SANDBOX_EXEC;
use crate::search_tools::{code_search_tool, find_files_tool, repo_map_tool};
use crate::types::{CodingPermissionAuthority, CodingToolPackOptions};

pub const CODING_TOOL_NAMES: &[&str] = &[
    "read_file",
    "list_dir",
    "find_files",
    "code_search",
    "repo_map",
    "git_inspect",
    "edit_file",
    "write_file",
    "run_command",
    "run_tests",
    "submit_plan",
];

fn invalid_configuration(message: impl Into<String>) -> CodingError {
    CodingError::new("invalid_configuration", message)
}

/// Merge overrides over the defaults and validate, mirroring
/// `resolveLimits`: every bound must be a positive number.
fn resolve_limits(
    partial: &crate::limits::CodingLimitsOverride,
) -> Result<CodingLimits, CodingError> {
    let resolved = partial.resolve();
    let checks: [(&str, f64); 34] = [
        ("readMaxLines", resolved.read_max_lines as f64),
        ("readMaxBytes", resolved.read_max_bytes as f64),
        ("readMaxFileBytes", resolved.read_max_file_bytes as f64),
        ("listMaxEntries", resolved.list_max_entries as f64),
        ("findDefaultResults", resolved.find_default_results as f64),
        ("findMaxResults", resolved.find_max_results as f64),
        ("searchMaxFiles", resolved.search_max_files as f64),
        ("searchMaxFileBytes", resolved.search_max_file_bytes as f64),
        (
            "searchMaxTotalBytes",
            resolved.search_max_total_bytes as f64,
        ),
        (
            "searchMaxOutputBytes",
            resolved.search_max_output_bytes as f64,
        ),
        ("searchTimeBudgetMs", resolved.search_time_budget_ms as f64),
        (
            "searchDefaultMatches",
            resolved.search_default_matches as f64,
        ),
        ("searchMaxMatches", resolved.search_max_matches as f64),
        ("searchMaxContext", resolved.search_max_context as f64),
        ("searchMaxLineChars", resolved.search_max_line_chars as f64),
        (
            "searchMaxPatternChars",
            resolved.search_max_pattern_chars as f64,
        ),
        ("ignoreFileMaxBytes", resolved.ignore_file_max_bytes as f64),
        (
            "repoMapDefaultDepth",
            resolved.repo_map_default_depth as f64,
        ),
        ("repoMapMaxDepth", resolved.repo_map_max_depth as f64),
        ("repoMapMaxEntries", resolved.repo_map_max_entries as f64),
        (
            "repoMapManifestExcerptBytes",
            resolved.repo_map_manifest_excerpt_bytes as f64,
        ),
        ("editMaxFileBytes", resolved.edit_max_file_bytes as f64),
        ("editMaxDiffBytes", resolved.edit_max_diff_bytes as f64),
        ("writeMaxBytes", resolved.write_max_bytes as f64),
        ("gitMaxOutputBytes", resolved.git_max_output_bytes as f64),
        ("gitMaxCount", resolved.git_max_count as f64),
        ("gitTimeoutMs", resolved.git_timeout_ms as f64),
        (
            "commandDefaultTimeoutSec",
            resolved.command_default_timeout_sec as f64,
        ),
        (
            "commandMaxTimeoutSec",
            resolved.command_max_timeout_sec as f64,
        ),
        (
            "commandTailMaxLines",
            resolved.command_tail_max_lines as f64,
        ),
        (
            "commandTailMaxBytes",
            resolved.command_tail_max_bytes as f64,
        ),
        ("commandMaxLogBytes", resolved.command_max_log_bytes as f64),
        ("commandKillGraceMs", resolved.command_kill_grace_ms as f64),
        ("toolTimeoutMs", resolved.tool_timeout_ms as f64),
    ];
    for (name, value) in checks {
        if !value.is_finite() || value <= 0.0 {
            return Err(invalid_configuration(format!(
                "coding limit \"{name}\" must be a positive number"
            )));
        }
    }
    Ok(resolved)
}

/// The coding tool pack. Build once per workspace; share the `Arc` between
/// runs.
pub struct CodingToolPack {
    env: Arc<PackEnv>,
    permission: Option<Arc<dyn CodingPermissionAuthority>>,
}

impl CodingToolPack {
    /// Mirrors `createCodingToolPack` — including its construction-time
    /// configuration errors.
    pub async fn new(options: CodingToolPackOptions) -> Result<Arc<Self>, CodingError> {
        let root = options.workspace.root.clone();
        if !root.starts_with('/') {
            return Err(invalid_configuration(
                "workspace.root must be an absolute path",
            ));
        }
        if tokio::runtime::Handle::try_current().is_ok() {
            // A crashed agent never ran its teardown; clear what it left,
            // once per process. Kicked off here and awaited by the
            // modifying path (`run_command`) through `startup_recovery`,
            // so a crashed agent's orphans are swept before any new
            // effect starts (ADR 0020). The `OnceCell` keeps the
            // process-once semantics; this spawn only warms it.
            tokio::spawn(async {
                startup_recovery().await;
            });
        }
        let limits = resolve_limits(&options.limits)?;
        let coordination_wait_ms = options.coordination_wait_ms.unwrap_or_else(|| {
            std::cmp::min(
                20_000,
                (limits.tool_timeout_ms as f64 * 0.66).floor() as u64,
            )
        });
        if coordination_wait_ms == 0 || coordination_wait_ms >= limits.tool_timeout_ms {
            return Err(invalid_configuration(
                "coordinationWaitMs must be positive and below the toolTimeoutMs limit",
            ));
        }
        let platform = options.platform.clone().unwrap_or_else(|| {
            if cfg!(target_os = "macos") {
                "darwin".to_owned()
            } else if cfg!(target_os = "windows") {
                "win32".to_owned()
            } else {
                std::env::consts::OS.to_owned()
            }
        });
        let env = Arc::new(PackEnv::new(crate::context::EnvInit {
            workspace_root: root,
            limits,
            allow_sensitive: options.allow_sensitive,
            clock: options
                .clock
                .clone()
                .unwrap_or_else(crate::types::system_clock),
            sandbox: options.sandbox,
            network: options.network,
            allow_git_writes: options.allow_git_writes,
            test_commands: options.test_commands.clone(),
            platform,
            sandbox_exec_path: options
                .sandbox_exec_path
                .clone()
                .unwrap_or_else(|| SANDBOX_EXEC.to_owned()),
            coordinator: options
                .coordinator
                .clone()
                .unwrap_or_else(default_workspace_coordinator),
            coordination_wait_ms,
        }));
        Ok(Arc::new(Self {
            env,
            permission: options.permission.clone(),
        }))
    }

    pub fn env(&self) -> &Arc<PackEnv> {
        &self.env
    }

    /// Drain in-flight command executions of THIS workspace: terminate every
    /// still-running process group through the same teardown path the
    /// execution driver uses, then await the drivers' completion (bounded).
    /// Host and test shutdown call this so a runtime never exits with marked
    /// children alive (ADR 0020: effects without a settled cleanup are never
    /// left to the crash sweep of the next boot if this process can still
    /// help). Commands of other workspaces are left to their own pack.
    pub async fn shutdown(&self) {
        if let Ok(root) = self.env.root().await {
            crate::process::drain_workspace(&root).await;
        }
    }

    /// Deletes the private scratch directory (TS `dispose`). Drains live
    /// executions first: `dispose` must not leave live children behind.
    pub async fn dispose(&self) {
        if let Ok(root) = self.env.root().await {
            crate::process::drain_workspace(&root).await;
        }
        self.env.dispose().await;
    }
}

#[async_trait]
impl ToolContributor for CodingToolPack {
    fn namespace(&self) -> &str {
        CODING_NAMESPACE
    }

    async fn contribute(
        &self,
        _context: &ToolContributionContext,
    ) -> agentkit_host::HostResult<Vec<ContributedTool>> {
        let env = Arc::clone(&self.env);
        // It offers every tool on every run; the guard decides.
        Ok(vec![
            ContributedTool::Read(read_file_tool(&env)),
            ContributedTool::Read(list_dir_tool(&env)),
            ContributedTool::Read(find_files_tool(&env)),
            ContributedTool::Read(code_search_tool(&env)),
            ContributedTool::Read(repo_map_tool(&env)),
            ContributedTool::Read(git_inspect_tool(&env)),
            ContributedTool::DirectWrite(edit_file_tool(&env)),
            ContributedTool::DirectWrite(write_file_tool(&env)),
            ContributedTool::DirectWrite(run_command_tool(&env)),
            ContributedTool::DirectWrite(run_tests_tool(&env)),
            // Always offered: a session that enters plan mode needs the way
            // out present in the same run that enters it. Outside plan mode
            // the guard allows it and the tool reports, truthfully, what was
            // granted.
            ContributedTool::Read(submit_plan_tool(
                env.limits.tool_timeout_ms,
                self.permission.clone(),
            )),
        ])
    }
}

/// Convenience alias for the TS factory name.
pub async fn create_coding_tool_pack(
    options: CodingToolPackOptions,
) -> Result<Arc<CodingToolPack>, CodingError> {
    CodingToolPack::new(options).await
}

// Re-exported so hosts can cancel a pack's in-flight work through the same
// token type the contributor context carries.
pub type PackCancellation = CancellationToken;
