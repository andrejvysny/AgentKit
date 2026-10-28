//! Confined coding tool pack: filesystem read/search/edit tools, read-only
//! git inspection and sandboxed command/test execution, plus the guard that
//! decides which of them a run may use. Rust port of
//! `packages/toolpacks-coding` (macOS-first sandbox via `sandbox-exec`).
//! See `docs/adr/0018-coding-tool-pack.md`.

pub mod args;
pub mod command_policy;
pub mod context;
pub mod coordination;
pub mod coordination_lockdir;
pub mod diff;
pub mod edit_engine;
pub mod errors;
pub mod exec_tools;
pub mod fs_tools;
pub mod fs_util;
pub mod git_tool;
pub mod guard;
pub mod limits;
pub mod mutation_queue;
pub mod pack;
pub mod paths;
pub mod plan_tool;
pub mod process;
pub mod process_tree;
pub mod sandbox;
pub mod search;
pub mod search_tools;
pub mod truncate;
pub mod types;
pub mod version_ledger;

pub use args::{as_record, opt_bool, opt_int, opt_string, req_string};
pub use command_policy::{
    CommandClass, HighRiskCommandGuard, ShellArgs, classify_shell_command,
    coding_standing_approval_key, high_risk_refusal, is_shell_tool, shell_args_of,
};
pub use context::{
    EnvInit, Outcome, PackEnv, ToolEffect, ToolSpec, define_tool, failure, to_coding_error,
};
pub use coordination::{
    AcquireFileInput, AcquireWorkspaceInput, CoordinationWait, CoordinatorObserver,
    InProcessCoordinator, LeaseHolder, WaitKind, WorkspaceCoordinator, WorkspaceLease,
    canonical_file_key, canonical_root_key, create_in_process_workspace_coordinator,
    default_in_process_coordinator, default_workspace_coordinator,
};
pub use coordination_lockdir::{
    LockDirCoordinatorOptions, OnError, create_lockdir_workspace_coordinator,
};
pub use diff::{UnifiedDiff, unified_diff};
pub use edit_engine::{
    AppliedEdits, Edit, Eol, SplitBom, apply_edits, dominant_eol, normalize_for_fuzzy_match,
    normalize_to_lf, split_bom,
};
pub use errors::{
    CodingError, file_changed_error, file_created_elsewhere_error, file_not_read_error,
};
pub use fs_util::{
    atomic_write, create_if_absent, decode_utf8, looks_binary, read_regular_file, sha256,
};
pub use guard::{
    CODING_NAMESPACE, CodingPermissionGuard, CodingPermissionGuardOptions, PLAN_MODE_BLOCKED_HINT,
};
pub use limits::{CodingLimits, CodingLimitsOverride};
pub use mutation_queue::with_file_mutation_queue;
pub use pack::{CODING_TOOL_NAMES, CodingToolPack, create_coding_tool_pack};
pub use paths::{
    ConfineOptions, ConfinedPath, PathAccess, canonical_root, confine, is_git_segment,
    is_sensitive_path, is_sensitive_segment, resolve_inside_root, sensitive_git_excludes,
};
pub use plan_tool::{
    SUBMIT_PLAN_CAPABILITY, SUBMIT_PLAN_TOOL_NAME, args_sha256_of, submit_plan_tool,
};
pub use process::{
    RunResult, RunSpec, Teardown, TeardownScan, kill_process_tree, run_bounded, sweep_orphans,
};
pub use sandbox::{
    SANDBOX_EXEC, SandboxEnvironment, SandboxPolicy, sandbox_available, sandbox_profile,
    toolchain_paths,
};
pub use truncate::{
    TruncatedBy, Truncation, TruncationLimits, format_size, split_lines, truncate_head,
    truncate_tail,
};
pub use types::{
    Clock, CodingMode, CodingPermissionAuthority, CodingRole, CodingToolPackOptions,
    CodingWorkspace, PermissionContext, SandboxMode, system_clock,
};
pub use version_ledger::{LedgerEntry, clear_version_ledger, expected, record_read, record_write};
