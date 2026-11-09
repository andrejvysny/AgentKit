//! VM sandbox routing: every sandbox-bound tool call goes through the
//! session's `SandboxWorkspaceIo` and NEVER touches host I/O (S06).
//!
//! Reached only when `PackEnv::vm` is `Some`; the host code paths stay
//! bit-identical when it is `None`. Invariants enforced here:
//!
//! * **No host fallback.** When a session is present, a sandbox error is
//!   returned as a tool error carrying the stable `SandboxErrorCode`
//!   (§6.5); nothing is retried on the host, before or after dispatch.
//! * **Digest discipline (S07).** Grants are built only via
//!   `VmSandboxSession::grant_scaffold` with the zero-form
//!   `intended_request_digest`, and `ExecRequest.request_digest` is sent
//!   zero-form too: the `SandboxWorkspaceIo` *implementation* (the host
//!   bridge) is the single party allowed to fill real digests, always via
//!   `canonical::request_digest_of`. The pack passes its scaffold through
//!   unchanged — it never computes or copies the digest authority.
//! * **Guest bytes are untrusted (S17).** Guest file bytes are
//!   digest-verified against the reply's own SHA-256, and command
//!   stdout/stderr run through the same truncation/limits pipeline as
//!   host-produced output (`truncate_tail` + `describe_truncation` +
//!   `command_status`). Guest output is model-facing data only; it never
//!   authorizes anything.
//! * **Writer lease (S09).** Mutating calls pass `PackEnv::vm_writer_lease`
//!   — `None` in v1 — and serialize writers through the existing
//!   coordinator exactly like the host path (file leases for `write_file`
//!   /`edit_file`, the exclusive workspace lease plus the pre-dispatch
//!   execution intent for commands).
//! * **Uncertainty is reconciliation, not retry (S14).** A command whose
//!   completion cannot be proven after `exec_cancel` is reported
//!   `sandbox_execution_uncertain` and its ownership intent is retained,
//!   never cleared.

use std::collections::{HashMap, VecDeque};
use std::process::id as process_id;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use regex::RegexBuilder;
use serde_json::{Map, Value, json};

use agentkit_core::ToolContext;

use agentkit_sandbox_contracts::api::{
    EntryKind, ExecArgv, ExecEventKind, ExecRequest, ExecutionHandle, GitCommand, GitInspectQuery,
    ListOptions, SearchMatch, SearchQuery,
};
use agentkit_sandbox_contracts::error::{RetryClass, SandboxError, SandboxErrorCode};
use agentkit_sandbox_contracts::grant::{GuestEnvironment, WorkspaceGrant};
use agentkit_sandbox_contracts::ids::{
    ArtifactDigest, AttemptId, ExecutionId, RequestDigest, ToolCallId,
};
use agentkit_sandbox_contracts::limits as sandbox_limits;
use agentkit_sandbox_contracts::path::GuestPath;
use agentkit_sandbox_contracts::policy::SandboxCapability;
use agentkit_sandbox_contracts::session::{CallCorrelation, VmSandboxSession};

use crate::context::{Outcome, PackEnv};
use crate::coordination::{
    AcquireFileInput, AcquireWorkspaceInput, ExecutionIntent, LeaseHolder, WorkspaceLease,
    now_ms_u64,
};
use crate::diff::unified_diff;
use crate::edit_engine::{Edit, apply_edits, split_bom};
use crate::errors::{
    CodingError, file_changed_error, file_created_elsewhere_error, file_not_read_error,
};
use crate::exec_tools::{IntentGuard, ShellRequest, command_not_started, command_status};
use crate::fs_tools::{INVALID_UTF8_WARNING, LIST_COLLECT_SLACK, render_listing};
use crate::fs_util::{decode_utf8, looks_binary, sha256};
use crate::limits::CodingLimits;
use crate::paths::is_sensitive_segment;
use crate::process::{describe_truncation, startup_recovery};
use crate::search::{
    IGNORE_FILES, IgnoreRule, clip_chars, compare_segments, compile_glob, excluded, glob_matches,
    parse_ignore_rule, seconds,
};
use crate::truncate::{TruncationLimits, format_size, truncate_head, truncate_tail};
use crate::version_ledger as ledger;

/// How long the event pump waits between `exec_events` polls.
const EVENT_POLL: Duration = Duration::from_millis(20);
/// Extra wall time beyond the command timeout to observe the guest's own
/// `Exited` before asking to cancel.
const DISPATCH_GRACE: Duration = Duration::from_millis(5_000);
/// Wall time allowed to reach a terminal event after `exec_cancel`; the
/// cancel is *requested*, never trusted as stopped (S14).
const CANCEL_GRACE: Duration = Duration::from_millis(10_000);

/// The workspace-relative rendering of a guest path, used in
/// model-facing text (mirrors the host `relative(workspace_root, path)`).
const WORKSPACE: &str = "/workspace";

/// One routed tool call: the session plus the correlation and clock its
/// grants are minted against.
pub(crate) struct VmCall<'a> {
    pub(crate) session: &'a Arc<VmSandboxSession>,
    pub(crate) correlation: CallCorrelation,
    pub(crate) now_ms: u64,
}

impl<'a> VmCall<'a> {
    pub(crate) fn new(env: &'a PackEnv, ctx: &ToolContext) -> Option<Self> {
        let session = env.vm.as_ref()?;
        Some(Self {
            session,
            correlation: correlation_of(ctx),
            now_ms: u64::try_from((env.clock)()).unwrap_or(0),
        })
    }

    /// Mint the per-request grant scaffold. The digest stays in its zero
    /// form: only the `SandboxWorkspaceIo` implementation fills it
    /// immediately before dispatch (single authority
    /// `canonical::request_digest_of`). Mutating grants carry the injected
    /// writer lease (`PackEnv::vm_writer_lease`, `None` in v1).
    fn grant(
        &self,
        env: &PackEnv,
        capability: SandboxCapability,
        permitted_paths: Vec<GuestPath>,
        cwd: Option<GuestPath>,
    ) -> WorkspaceGrant {
        let writer_lease = match capability {
            SandboxCapability::SandboxWrite | SandboxCapability::ProcessExec => {
                env.vm_writer_lease.clone()
            }
            _ => None,
        };
        self.session.grant_scaffold(
            capability,
            &self.correlation,
            writer_lease,
            permitted_paths,
            cwd,
            self.now_ms,
        )
    }
}

/// The run-loop identities the pack actually carries. `ToolContext` has no
/// task id, so `task_id` stays `None` — never fabricated. Ids outside the
/// contract grammar are dropped rather than rewritten.
pub(crate) fn correlation_of(ctx: &ToolContext) -> CallCorrelation {
    CallCorrelation {
        tool_call_id: ctx
            .tool_call_id
            .as_deref()
            .and_then(|value| ToolCallId::new(value).ok()),
        task_id: None,
        attempt_id: ctx
            .attempt_id
            .as_deref()
            .and_then(|value| AttemptId::new(value).ok()),
    }
}

fn retry_text(class: RetryClass) -> &'static str {
    match class {
        RetryClass::SafeAfterFreshDecision => "safe-after-fresh-decision",
        RetryClass::ReplayableByIdentity => "replayable-by-identity",
        RetryClass::UncertainReconcile => "uncertain-reconcile",
        RetryClass::RefusedUntilReconciled => "refused-until-reconciled",
    }
}

/// Surface the stable machine-readable sandbox code (§6.5) in the tool
/// error: the code IS the sandbox code string and the message repeats it,
/// so no consumer can miss the distinction. This is the terminal shape —
/// there is no host retry behind it (S06).
pub(crate) fn sandbox_coding_error(error: &SandboxError) -> CodingError {
    let code = error.code.as_str();
    CodingError::new(
        code,
        format!(
            "{code}: {} (retry class: {})",
            error.message,
            retry_text(error.code.retry_class())
        ),
    )
}

/// `map_err` adapter: the io traits hand over the error by value.
fn sandbox_coding_error_owned(error: SandboxError) -> CodingError {
    sandbox_coding_error(&error)
}

/// Map a tool-input path (workspace-relative, `.`-rooted, host-absolute
/// under the configured root, or an explicit `/workspace`-rooted guest
/// path) onto a validated [`GuestPath`]. Host path resolution never runs:
/// the guest tree is the only filesystem here.
pub(crate) fn guest_path_of(env: &PackEnv, raw: &str) -> Result<GuestPath, CodingError> {
    let invalid = |reason: String| CodingError::new("invalid_path", format!("{raw}: {reason}"));
    if raw.contains('\0') {
        return Err(invalid("NUL in path".to_owned()));
    }
    // An explicit guest path passes the contract's own validation.
    if raw == WORKSPACE || raw.starts_with(&format!("{WORKSPACE}/")) {
        let path = GuestPath::parse(raw).map_err(|error| invalid(error.to_string()))?;
        check_sensitive(env, &path)?;
        return Ok(path);
    }
    // A host-absolute path inside the configured workspace root maps to
    // the same relative guest path; any other absolute path is refused.
    let root = env.workspace_root.trim_end_matches('/');
    let relative = if raw == root {
        ""
    } else if let Some(rest) = raw.strip_prefix(&format!("{root}/")) {
        rest
    } else if raw.starts_with('/') {
        return Err(invalid(
            "absolute path outside the workspace root".to_owned(),
        ));
    } else {
        raw
    };
    let mut parts: Vec<&str> = Vec::new();
    for segment in relative.split('/') {
        match segment {
            "" | "." => continue,
            ".." => return Err(invalid("path escapes the workspace root".to_owned())),
            other => parts.push(other),
        }
    }
    if parts
        .iter()
        .any(|part| part.len() > sandbox_limits::MAX_PATH_SEGMENT_BYTES)
    {
        return Err(invalid("path segment too long".to_owned()));
    }
    let joined = if parts.is_empty() {
        WORKSPACE.to_owned()
    } else {
        format!("{WORKSPACE}/{}", parts.join("/"))
    };
    let path = GuestPath::parse(&joined).map_err(|error| invalid(error.to_string()))?;
    check_sensitive(env, &path)?;
    Ok(path)
}

/// The pack's credential-path policy stays in force for guest paths too:
/// a sensitive segment is refused before any io call unless explicitly
/// allowed.
fn check_sensitive(env: &PackEnv, path: &GuestPath) -> Result<(), CodingError> {
    if env.allow_sensitive {
        return Ok(());
    }
    for segment in path.components() {
        if is_sensitive_segment(segment) {
            return Err(CodingError::new(
                "sensitive_path",
                format!("{} is a credential path; not allowed", path.as_str()),
            ));
        }
    }
    Ok(())
}

/// The workspace-relative rendering used in model-facing text.
fn guest_relative(path: &GuestPath) -> String {
    if path.as_str() == WORKSPACE {
        return String::new();
    }
    path.as_str()
        .strip_prefix(&format!("{WORKSPACE}/"))
        .unwrap_or(path.as_str())
        .to_owned()
}

fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

fn digest_from_hex(hex: &str) -> Result<ArtifactDigest, CodingError> {
    ArtifactDigest::from_hex(hex)
        .map_err(|_| CodingError::new("io_error", "internal digest conversion"))
}

fn throw_if_aborted(env: &PackEnv, ctx: &ToolContext) -> Result<(), CodingError> {
    if env.is_closed() {
        return Err(CodingError::new("cancelled", "coding pack closed"));
    }
    if ctx.cancellation.is_cancelled() {
        return Err(CodingError::new("aborted", "the call was aborted"));
    }
    Ok(())
}

fn slice_utf16(value: &str, max_units: usize) -> String {
    let units: Vec<u16> = value.encode_utf16().take(max_units).collect();
    String::from_utf16_lossy(&units)
}

fn guest_signal_label(sig: i32) -> String {
    #[cfg(unix)]
    {
        crate::process::signal_name(sig)
    }
    #[cfg(not(unix))]
    {
        format!("SIG{sig}")
    }
}

// ---------------------------------------------------------------------------
// File reads
// ---------------------------------------------------------------------------

/// Pull the whole (bounded) file from the guest: page through `next_from`,
/// recompute each page's SHA-256 locally and refuse unless it matches the
/// reply's own digest; a reply whose size or paging is not self-consistent
/// fails the integrity check instead of returning a supposedly stable
/// digest (S17).
pub(crate) async fn vm_read_bytes(
    call: &VmCall<'_>,
    env: &PackEnv,
    path: &GuestPath,
    cap: u64,
) -> Result<Vec<u8>, CodingError> {
    let grant = call.grant(
        env,
        SandboxCapability::SandboxRead,
        vec![path.clone()],
        None,
    );
    let mut bytes: Vec<u8> = Vec::new();
    let mut from: u64 = 0;
    let mut expected_total: Option<u64> = None;
    // Bounded paging: one reply per reply-bound window plus slack; a
    // guest that never progresses is a protocol violation, not a loop.
    let page = sandbox_limits::MAX_READ_REPLY_BYTES.max(1);
    let max_pages = (cap as usize / page) + 4;
    for _ in 0..max_pages.max(2) {
        let reply = call
            .session
            .io
            .read(&grant, path, Some(from), None)
            .await
            .map_err(sandbox_coding_error_owned)?;
        if let Some(total) = expected_total {
            if total != reply.total_size {
                return Err(CodingError::new(
                    "sandbox_artifact_invalid",
                    "file changed during read; the digest cannot be proven stable",
                ));
            }
        } else {
            expected_total = Some(reply.total_size);
        }
        if reply.total_size > cap {
            return Err(CodingError::new(
                "file_too_large",
                format!("{path} is {} bytes; the limit is {cap}", reply.total_size),
            ));
        }
        if reply.from != from {
            return Err(CodingError::new(
                "sandbox_protocol_incompatible",
                "read reply does not start at the requested offset",
            ));
        }
        let decoded = b64().decode(&reply.data_b64).map_err(|_| {
            CodingError::new("sandbox_artifact_invalid", "read payload is not base64")
        })?;
        if decoded.len() > sandbox_limits::MAX_READ_REPLY_BYTES {
            return Err(CodingError::new(
                "sandbox_protocol_incompatible",
                "read reply exceeds the reply bound",
            ));
        }
        if ArtifactDigest::of_bytes(&decoded) != reply.sha256 {
            return Err(CodingError::new(
                "sandbox_artifact_invalid",
                "read digest mismatch: the guest reply does not hash to its own sha256",
            ));
        }
        bytes.extend_from_slice(&decoded);
        match reply.next_from {
            Some(next) if next > from => from = next,
            Some(_) => {
                return Err(CodingError::new(
                    "sandbox_protocol_incompatible",
                    "read paging made no progress",
                ));
            }
            None => {
                if reply.truncated {
                    return Err(CodingError::new(
                        "sandbox_protocol_incompatible",
                        "truncated read reply without a continuation offset",
                    ));
                }
                let total = expected_total.unwrap_or(bytes.len() as u64);
                if total != bytes.len() as u64 {
                    return Err(CodingError::new(
                        "sandbox_artifact_invalid",
                        "file changed during read; the digest cannot be proven stable",
                    ));
                }
                return Ok(bytes);
            }
        }
    }
    Err(CodingError::new(
        "sandbox_protocol_incompatible",
        "read paging did not converge",
    ))
}

/// A read that maps "absent" to `None` instead of an error, so
/// create-vs-overwrite can branch like the host path.
async fn vm_try_read(
    call: &VmCall<'_>,
    env: &PackEnv,
    path: &GuestPath,
) -> Result<Option<Vec<u8>>, CodingError> {
    let cap = *[
        env.limits.edit_max_file_bytes,
        env.limits.read_max_file_bytes,
        env.limits.write_max_bytes as u64,
    ]
    .iter()
    .max()
    .unwrap_or(&0);
    match vm_read_bytes(call, env, path, cap).await {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.code == SandboxErrorCode::NotFound.as_str() => Ok(None),
        Err(error) => Err(error),
    }
}

/// `read_file` under a VM session: every file (or per-file failure) is
/// produced from `io.read` bytes through the shared formatting pipeline
/// (`format_file_read`), so numbering, caps, notices and the version
/// ledger behave exactly like the host tool.
pub(crate) async fn read_file(
    env: &Arc<PackEnv>,
    ctx: &ToolContext,
    paths: &[String],
    offset: i64,
    limit: Option<i64>,
    single: bool,
) -> Result<Outcome, CodingError> {
    let call = VmCall::new(env, ctx).expect("vm session present");
    throw_if_aborted(env, ctx)?;
    async fn read_one(
        env: &PackEnv,
        call: &VmCall<'_>,
        ctx: &ToolContext,
        path: &str,
        offset: i64,
        limit: Option<i64>,
        share: usize,
    ) -> Result<crate::fs_tools::FileRead, CodingError> {
        let target = guest_path_of(env, path)?;
        let bytes = vm_read_bytes(call, env, &target, env.limits.read_max_file_bytes).await?;
        crate::fs_tools::format_file_read(
            env,
            path,
            target.as_str(),
            offset,
            limit,
            share,
            ctx.chat_id.as_deref(),
            bytes,
        )
    }
    if single {
        let one = read_one(
            env,
            &call,
            ctx,
            &paths[0],
            offset,
            limit,
            env.limits.read_max_bytes,
        )
        .await?;
        let mut details = json!({
            "path": paths[0],
            "sha256": one.sha256,
            "totalLines": one.total_lines,
        });
        if one.invalid_utf8 {
            details["encoding"] = json!("invalid-utf8");
        }
        return Ok(Outcome {
            ok: true,
            text: one.text,
            summary: Some(one.summary),
            truncated: one.truncated,
            warnings: if one.invalid_utf8 {
                vec![INVALID_UTF8_WARNING.to_owned()]
            } else {
                vec![]
            },
            details: Some(details),
        });
    }
    let share = env.limits.read_max_bytes / paths.len();
    let mut blocks: Vec<String> = Vec::new();
    let mut files: Vec<Value> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut failed = 0usize;
    let mut truncated = false;
    for path in paths {
        throw_if_aborted(env, ctx)?;
        match read_one(env, &call, ctx, path, offset, limit, share).await {
            Ok(one) => {
                blocks.push(format!("==> {path} <==\n{}", one.text));
                truncated = truncated || one.truncated;
                if one.invalid_utf8 {
                    warnings.push(format!("{path}: {INVALID_UTF8_WARNING}"));
                }
                let mut file = json!({
                    "path": path,
                    "sha256": one.sha256,
                    "totalLines": one.total_lines,
                });
                if one.invalid_utf8 {
                    file["encoding"] = json!("invalid-utf8");
                }
                files.push(file);
            }
            Err(error) => {
                failed += 1;
                blocks.push(format!("==> {path} <==\n{}: {}", error.code, error.message));
                files.push(json!({ "path": path, "error": error.code }));
            }
        }
    }
    Ok(Outcome {
        ok: failed < paths.len(),
        text: blocks.join("\n\n"),
        summary: Some(format!(
            "read {} of {} files",
            paths.len() - failed,
            paths.len()
        )),
        truncated,
        warnings,
        details: Some(json!({ "files": files })),
    })
}

// ---------------------------------------------------------------------------
// Guest ignore rules (guest bytes, host engine)
// ---------------------------------------------------------------------------

/// Read one directory's own ignore files from the guest and compile them
/// exactly like the host engine; a missing, unreadable, oversized or
/// digest-inconsistent file is skipped, mirroring `search::ignore_rules`.
async fn guest_own_rules(
    call: &VmCall<'_>,
    env: &PackEnv,
    cache: &mut HashMap<String, Vec<IgnoreRule>>,
    directory: &GuestPath,
) -> Vec<IgnoreRule> {
    if let Some(rules) = cache.get(directory.as_str()) {
        return rules.clone();
    }
    let mut rules: Vec<IgnoreRule> = Vec::new();
    for name in IGNORE_FILES {
        // `.git/info/exclude` is a multi-segment relative name; `join`
        // validates every segment.
        let Ok(target) = directory.join(name) else {
            continue;
        };
        let grant = call.grant(
            env,
            SandboxCapability::SandboxRead,
            vec![target.clone()],
            None,
        );
        let Ok(reply) = call.session.io.read(&grant, &target, Some(0), None).await else {
            continue;
        };
        let Ok(bytes) = b64().decode(&reply.data_b64) else {
            continue;
        };
        if bytes.len() as u64 > env.limits.ignore_file_max_bytes {
            continue;
        }
        if ArtifactDigest::of_bytes(&bytes) != reply.sha256 {
            continue;
        }
        if reply.next_from.is_some() {
            // An oversized ignore file is not parsed, exactly like the
            // host engine's capped read.
            continue;
        }
        let text = String::from_utf8_lossy(&bytes);
        for line in text.split('\n') {
            if let Some((pattern, negate, directory_only)) = parse_ignore_rule(line) {
                rules.push(IgnoreRule {
                    base: directory.as_str().to_owned(),
                    pattern,
                    negate,
                    directory_only,
                });
            }
        }
    }
    cache.insert(directory.as_str().to_owned(), rules.clone());
    rules
}

/// Rules in force for `directory`: every directory from the workspace root
/// down to (and including) the target contributes its own file.
async fn guest_chain_rules(
    call: &VmCall<'_>,
    env: &PackEnv,
    cache: &mut HashMap<String, Vec<IgnoreRule>>,
    directory: &GuestPath,
) -> Vec<IgnoreRule> {
    let mut all = Vec::new();
    let Ok(mut current) = GuestPath::parse(WORKSPACE) else {
        return all;
    };
    all.extend(guest_own_rules(call, env, cache, &current).await);
    for segment in guest_relative(directory)
        .split('/')
        .filter(|s| !s.is_empty())
    {
        let Ok(next) = current.join(segment) else {
            break;
        };
        current = next;
        all.extend(guest_own_rules(call, env, cache, &current).await);
    }
    all
}

/// One BFS pass over the guest tree with the host walk's rules: `.git` and
/// `node_modules`, symlinks and (unless allowed) credential-name segments
/// are skipped, per-directory ignore rules exclude entries, and the scan
/// is bounded by entry count and wall clock like `search::Walk`.
struct GuestWalkEntry {
    rel: String,
    is_directory: bool,
}

struct GuestWalk {
    entries: Vec<GuestWalkEntry>,
    entries_exhausted: bool,
    timed_out: bool,
}

#[allow(clippy::too_many_arguments)] // mirrors the host Walk's option surface
async fn guest_walk(
    call: &VmCall<'_>,
    env: &PackEnv,
    ctx: &ToolContext,
    root: &GuestPath,
    include_directories: bool,
    max_depth: Option<usize>,
    entry_cap: usize,
    deadline: i64,
) -> Result<GuestWalk, CodingError> {
    let mut cache = HashMap::new();
    let mut queue: VecDeque<(GuestPath, Vec<IgnoreRule>, usize)> = VecDeque::new();
    let root_rules = guest_own_rules(call, env, &mut cache, root).await;
    queue.push_back((root.clone(), root_rules, 0));
    let mut entries: Vec<GuestWalkEntry> = Vec::new();
    let mut scanned = 0usize;
    let entries_exhausted = false;
    let mut timed_out = false;
    while let Some((directory, rules, depth)) = queue.pop_front() {
        if ctx.cancellation.is_cancelled() {
            return Err(CodingError::new("aborted", "the call was aborted"));
        }
        if (env.clock)() > deadline {
            timed_out = true;
            break;
        }
        let grant = call.grant(
            env,
            SandboxCapability::SandboxRead,
            vec![directory.clone()],
            None,
        );
        let reply = match call
            .session
            .io
            .list(
                &grant,
                &directory,
                ListOptions {
                    max_entries: sandbox_limits::MAX_LIST_ENTRIES as u32,
                    include_hidden: true,
                },
            )
            .await
        {
            Ok(reply) => reply,
            // A directory that cannot be listed is skipped, like an
            // unreadable directory in the host walk.
            Err(_) => continue,
        };
        for entry in reply.entries {
            scanned += 1;
            if scanned > env.limits.search_max_files {
                return Ok(GuestWalk {
                    entries,
                    entries_exhausted: true,
                    timed_out,
                });
            }
            if entry.name == ".git"
                || entry.name == "node_modules"
                || entry.kind == EntryKind::Symlink
                || (!env.allow_sensitive && is_sensitive_segment(&entry.name))
            {
                continue;
            }
            let Ok(full) = directory.join(&entry.name) else {
                continue;
            };
            let is_directory = entry.kind == EntryKind::Dir;
            if excluded(&rules, full.as_str(), is_directory) {
                continue;
            }
            let rel = guest_relative(&full);
            if is_directory {
                if max_depth.is_none_or(|max| depth + 1 < max) {
                    let mut child_rules = rules.clone();
                    child_rules.extend(guest_own_rules(call, env, &mut cache, &full).await);
                    queue.push_back((full.clone(), child_rules, depth + 1));
                }
                if include_directories {
                    entries.push(GuestWalkEntry {
                        rel,
                        is_directory: true,
                    });
                }
            } else {
                entries.push(GuestWalkEntry {
                    rel,
                    is_directory: false,
                });
            }
            if entries.len() >= entry_cap {
                return Ok(GuestWalk {
                    entries,
                    entries_exhausted,
                    timed_out,
                });
            }
        }
    }
    Ok(GuestWalk {
        entries,
        entries_exhausted,
        timed_out,
    })
}

// ---------------------------------------------------------------------------
// list_dir
// ---------------------------------------------------------------------------

/// `list_dir` under a VM session: `io.list` supplies the entries; the
/// sensitive filter, ignore filter, ordering, suffixes and notices run
/// through the host rendering pipeline (`render_listing`).
pub(crate) async fn list_dir(
    env: &Arc<PackEnv>,
    ctx: &ToolContext,
    path: &str,
    include_ignored: bool,
) -> Result<Outcome, CodingError> {
    let call = VmCall::new(env, ctx).expect("vm session present");
    throw_if_aborted(env, ctx)?;
    let directory = guest_path_of(env, path)?;
    let grant = call.grant(
        env,
        SandboxCapability::SandboxRead,
        vec![directory.clone()],
        None,
    );
    let collect_cap =
        (env.limits.list_max_entries + LIST_COLLECT_SLACK).min(sandbox_limits::MAX_LIST_ENTRIES);
    let reply = call
        .session
        .io
        .list(
            &grant,
            &directory,
            ListOptions {
                max_entries: collect_cap as u32,
                include_hidden: include_ignored,
            },
        )
        .await
        .map_err(sandbox_coding_error_owned)?;
    let mut cache = HashMap::new();
    let rules = if include_ignored {
        Vec::new()
    } else {
        guest_chain_rules(&call, env, &mut cache, &directory).await
    };
    let permitted: Vec<_> = reply
        .entries
        .iter()
        .filter(|entry| env.allow_sensitive || !is_sensitive_segment(&entry.name))
        .collect();
    let mut visible: Vec<(String, bool, bool)> = permitted
        .iter()
        .filter(|entry| {
            let full = format!(
                "{}/{}",
                directory.as_str().trim_end_matches('/'),
                entry.name
            );
            !excluded(&rules, &full, entry.kind == EntryKind::Dir)
        })
        .map(|entry| {
            (
                entry.name.clone(),
                entry.kind == EntryKind::Dir,
                entry.kind == EntryKind::Symlink,
            )
        })
        .collect();
    // UTF-16 ordering, exactly like the host listing.
    visible.sort_by(|left, right| left.0.encode_utf16().cmp(right.0.encode_utf16()));
    let hidden = reply.entries.len() - permitted.len();
    let ignored = permitted.len() - visible.len();
    let (text, shown, truncated) = render_listing(
        &visible,
        env.limits.list_max_entries,
        reply.truncated,
        collect_cap,
        hidden,
        ignored,
    );
    Ok(Outcome {
        ok: true,
        text,
        summary: Some(format!("listed {path} ({shown} entries)")),
        truncated,
        warnings: vec![],
        details: None,
    })
}

// ---------------------------------------------------------------------------
// write_file / edit_file
// ---------------------------------------------------------------------------

fn lease_holder(ctx: &ToolContext, tool: &str, label: &str) -> LeaseHolder {
    LeaseHolder {
        chat_id: ctx.chat_id.clone(),
        run_id: ctx.run_id.clone(),
        tool_call_id: ctx.tool_call_id.clone(),
        tool: tool.to_owned(),
        label: slice_utf16(label, 200),
    }
}

/// Stable identity of one routed mutating operation, mirroring the host
/// `file_execution_id` grammar (`{pid}:{stable-call-token}`, ADR 0020).
pub(crate) fn file_execution_id(ctx: &ToolContext) -> String {
    let token = match ctx.tool_call_id.as_deref() {
        Some(id) if !id.is_empty() && !id.chars().any(char::is_whitespace) => id.to_owned(),
        _ => uuid::Uuid::new_v4().simple().to_string(),
    };
    format!("{}:{}", process_id(), token)
}

/// Acquire the same coordinator file lease the host mutation path takes,
/// keyed by the guest path identity (S09: the coordinator serializes
/// writers host-side; the worker grant fences execution-side).
pub(crate) async fn acquire_guest_file_lease(
    env: &PackEnv,
    ctx: &ToolContext,
    tool: &str,
    target: &GuestPath,
) -> Result<WorkspaceLease, CodingError> {
    env.coordinator
        .acquire_file(AcquireFileInput {
            root: env.workspace_root.clone(),
            path: target.as_str().to_owned(),
            holder: lease_holder(ctx, tool, &guest_relative(target)),
            cancellation: Some(ctx.cancellation.clone()),
            max_wait_ms: Some(env.coordination_wait_ms),
            execution_id: Some(file_execution_id(ctx)),
        })
        .await
}

/// `write_file` under a VM session. The caller wrapper takes the
/// coordinator file lease exactly like the host path; the digest checks
/// mirror `write_file_body`, with the guest's `expected_sha256` CAS as the
/// atomic optimistic-concurrency guard. A digest conflict is a retryable
/// `file_changed` — never a host fallback.
pub(crate) async fn write_file(
    env: &Arc<PackEnv>,
    ctx: &ToolContext,
    path: &str,
    data: &[u8],
    explicit: &Option<String>,
) -> Result<Outcome, CodingError> {
    let call = VmCall::new(env, ctx).expect("vm session present");
    throw_if_aborted(env, ctx)?;
    let target = guest_path_of(env, path)?;
    let relative = guest_relative(&target);
    if data.len() > sandbox_limits::MAX_WRITE_BODY_BYTES {
        return Err(CodingError::new(
            "content_too_large",
            format!(
                "content is {} bytes; the guest write bound is {}",
                data.len(),
                sandbox_limits::MAX_WRITE_BODY_BYTES
            ),
        ));
    }
    let current = vm_try_read(&call, env, &target).await?;
    let digest = sha256(data);
    let existed = current.is_some();
    let expected = if explicit.is_some() {
        explicit.clone()
    } else if current.is_some() {
        let seen = ledger::expected(ctx.chat_id.as_deref(), target.as_str());
        if seen.is_none() || seen.as_ref().is_some_and(|entry| !entry.full) {
            return Err(file_not_read_error(&relative, seen.is_some()));
        }
        seen.map(|entry| entry.sha256)
    } else {
        None
    };
    let current_sha = current.as_deref().map(sha256);
    if let Some(current_sha) = &current_sha {
        let Some(expected) = expected else {
            return Err(file_not_read_error(&relative, false));
        };
        if expected != *current_sha {
            return Err(file_changed_error(&relative));
        }
        let grant = call.grant(
            env,
            SandboxCapability::SandboxWrite,
            vec![target.clone()],
            None,
        );
        let receipt = call
            .session
            .io
            .write(
                &grant,
                &target,
                data.to_vec(),
                Some(digest_from_hex(current_sha)?),
            )
            .await
            .map_err(|error| {
                // The guest's CAS fired (or the file vanished under us):
                // retryable conflict, never host fallback.
                if matches!(
                    error.code,
                    SandboxErrorCode::ArtifactInvalid | SandboxErrorCode::NotFound
                ) {
                    file_changed_error(&relative)
                } else {
                    sandbox_coding_error(&error)
                }
            })?;
        if receipt.sha256 != digest_from_hex(&digest)? {
            return Err(file_changed_error(&relative));
        }
        ledger::record_write(ctx.chat_id.as_deref(), target.as_str(), &digest);
        Ok(Outcome {
            ok: true,
            text: format!(
                "{} {relative} ({} bytes)\nsha256: {digest}",
                if existed { "Overwrote" } else { "Created" },
                data.len()
            ),
            summary: Some(format!(
                "{} {relative}",
                if existed { "overwrote" } else { "created" }
            )),
            truncated: false,
            warnings: vec![],
            details: Some(json!({ "path": relative, "sha256": digest })),
        })
    } else {
        // An expected version means "overwrite that file"; it is gone.
        if explicit.is_some() {
            return Err(file_changed_error(&relative));
        }
        let grant = call.grant(
            env,
            SandboxCapability::SandboxWrite,
            vec![target.clone()],
            None,
        );
        let receipt = call
            .session
            .io
            .write(&grant, &target, data.to_vec(), None)
            .await
            .map_err(sandbox_coding_error_owned)?;
        if !receipt.created {
            // Lost the create race: the target existed at swap time even
            // though our probe saw nothing.
            return Err(file_created_elsewhere_error(&relative));
        }
        if receipt.sha256 != digest_from_hex(&digest)? {
            return Err(file_changed_error(&relative));
        }
        ledger::record_write(ctx.chat_id.as_deref(), target.as_str(), &digest);
        Ok(Outcome {
            ok: true,
            text: format!(
                "Created {relative} ({} bytes)\nsha256: {digest}",
                data.len()
            ),
            summary: Some(format!("created {relative}")),
            truncated: false,
            warnings: vec![],
            details: Some(json!({ "path": relative, "sha256": digest })),
        })
    }
}
/// `edit_file` under a VM session: read-modify-write. The edit engine
/// (ambiguity checks, CRLF/BOM preservation) is the host engine run over
/// guest bytes; the write carries the digest we read as `expected_sha256`,
/// so the guest's own CAS refuses a stale edit — a conflict is a retryable
/// `file_changed`, never a host fallback.
pub(crate) async fn edit_file(
    env: &Arc<PackEnv>,
    ctx: &ToolContext,
    path: &str,
    edits: &[Edit],
    explicit: &Option<String>,
) -> Result<Outcome, CodingError> {
    let call = VmCall::new(env, ctx).expect("vm session present");
    throw_if_aborted(env, ctx)?;
    let target = guest_path_of(env, path)?;
    let relative = guest_relative(&target);
    let Some(buffer) = vm_try_read(&call, env, &target).await? else {
        return Err(CodingError::new(
            "not_found",
            format!("{path} does not exist; edit_file edits an existing file"),
        ));
    };
    if looks_binary(&buffer) {
        return Err(CodingError::new(
            "binary_file",
            format!("{path} looks binary; not edited"),
        ));
    }
    let before = sha256(&buffer);
    // A partial read is enough here: edits are anchored on oldText.
    let expected = match explicit {
        Some(value) => Some(value.clone()),
        None => ledger::expected(ctx.chat_id.as_deref(), target.as_str()).map(|entry| entry.sha256),
    };
    if expected.is_some() && expected.as_deref() != Some(before.as_str()) {
        return Err(file_changed_error(&relative));
    }
    let decoded = decode_utf8(&buffer).ok_or_else(|| {
        CodingError::new(
            "encoding_unsupported",
            format!("{path} is not valid UTF-8; not edited (editing would corrupt it)"),
        )
    })?;
    let mut split = split_bom(&decoded);
    // Pure CPU work over up to the file-size cap; keep it off the async
    // workers so a large file cannot stall other tools.
    let (text, edits_owned, path_owned) = (
        std::mem::take(&mut split.text),
        edits.to_vec(),
        path.to_owned(),
    );
    let applied =
        tokio::task::spawn_blocking(move || apply_edits(&text, &edits_owned, &path_owned))
            .await
            .map_err(|error| CodingError::new("io_error", error.to_string()))??;
    let out = format!("{}{}", split.bom, applied.output).into_bytes();
    throw_if_aborted(env, ctx)?;
    let grant = call.grant(
        env,
        SandboxCapability::SandboxWrite,
        vec![target.clone()],
        None,
    );
    let receipt = call
        .session
        .io
        .write(
            &grant,
            &target,
            out.clone(),
            Some(digest_from_hex(&before)?),
        )
        .await
        .map_err(|error| {
            if matches!(
                error.code,
                SandboxErrorCode::ArtifactInvalid | SandboxErrorCode::NotFound
            ) {
                file_changed_error(&relative)
            } else {
                sandbox_coding_error(&error)
            }
        })?;
    let after = sha256(&out);
    if receipt.sha256 != digest_from_hex(&after)? {
        return Err(file_changed_error(&relative));
    }
    ledger::record_write(ctx.chat_id.as_deref(), target.as_str(), &after);
    let patch = unified_diff(&relative, &applied.base_content, &applied.new_content);
    let cut = truncate_head(
        &patch.diff,
        TruncationLimits {
            max_lines: usize::MAX,
            max_bytes: env.limits.edit_max_diff_bytes,
        },
    );
    let diff_text = if cut.truncated {
        format!(
            "{}\n[diff truncated at {}]",
            cut.content,
            format_size(env.limits.edit_max_diff_bytes)
        )
    } else {
        patch.diff
    };
    Ok(Outcome {
        ok: true,
        text: format!(
            "Edited {relative} ({} edit{}, +{} -{})\nsha256: {after}\n{diff_text}",
            edits.len(),
            if edits.len() == 1 { "" } else { "s" },
            patch.added,
            patch.removed
        ),
        summary: Some(format!(
            "edited {relative} (+{} -{})",
            patch.added, patch.removed
        )),
        truncated: cut.truncated,
        warnings: vec![],
        details: Some(json!({
            "path": relative,
            "sha256": after,
            "diff": diff_text,
        })),
    })
}
// ---------------------------------------------------------------------------
// code_search / find_files / repo_map
// ---------------------------------------------------------------------------

fn no_matches_outcome(pattern: &str) -> Outcome {
    Outcome {
        ok: true,
        text: "No matches found".to_owned(),
        summary: Some(search_summary(pattern, 0)),
        truncated: false,
        warnings: vec![],
        details: None,
    }
}

fn search_summary(pattern: &str, matches: usize) -> String {
    format!(
        "{matches} match(es) for {}",
        pattern.chars().take(60).collect::<String>()
    )
}

fn build_search_regex(
    pattern: &str,
    literal: bool,
    ignore_case: bool,
) -> Result<regex::Regex, CodingError> {
    let escaped = if literal {
        let mut out = String::with_capacity(pattern.len());
        for char in pattern.chars() {
            if ".*+?^${}()|[]\\".contains(char) {
                out.push('\\');
            }
            out.push(char);
        }
        out
    } else {
        pattern.to_owned()
    };
    RegexBuilder::new(&escaped).case_insensitive(ignore_case).build().map_err(|_| {
        CodingError::new(
            "invalid_pattern",
            format!(
                "invalid regular expression {}; pass literal: true to search for the text itself",
                json_snippet(pattern)
            ),
        )
    })
}

fn json_snippet(value: &str) -> String {
    let clipped: String = value.chars().take(60).collect();
    format!("\"{}\"", clipped.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Render matched lines exactly like the host `search_text` loop: context
/// lines are plain `rel:n:text` entries, a match is shown whole or left
/// for the next page, and the stopped/more notices carry the same text.
fn format_search_output(
    all: &[SearchMatch],
    offset: usize,
    limit: usize,
    limits: &CodingLimits,
    reason: Option<String>,
    more_available: bool,
) -> (String, bool, usize) {
    let mut output: Vec<String> = Vec::new();
    let mut output_bytes = 0usize;
    let mut matches = 0usize;
    let mut output_full = false;
    for m in all.iter().skip(offset) {
        let mut entries: Vec<String> = Vec::new();
        let mut size = 0usize;
        let before_count = m.context_before.len() as u32;
        let start_line = m.line.saturating_sub(before_count);
        for (i, text) in m.context_before.iter().enumerate() {
            let entry = format!(
                "{}:{}:{}",
                m.path,
                start_line + i as u32,
                clip_chars(text, limits.search_max_line_chars)
            );
            size += entry.len() + 1;
            entries.push(entry);
        }
        let entry = format!(
            "{}:{}:{}",
            m.path,
            m.line,
            clip_chars(&m.text, limits.search_max_line_chars)
        );
        size += entry.len() + 1;
        entries.push(entry);
        for (i, text) in m.context_after.iter().enumerate() {
            let entry = format!(
                "{}:{}:{}",
                m.path,
                m.line + 1 + i as u32,
                clip_chars(text, limits.search_max_line_chars)
            );
            size += entry.len() + 1;
            entries.push(entry);
        }
        // A match is shown whole or left for the next page, never cut.
        if matches > 0 && output_bytes + size > limits.search_max_output_bytes {
            output_full = true;
            break;
        }
        output.extend(entries);
        output_bytes += size;
        matches += 1;
        if output_bytes >= limits.search_max_output_bytes {
            output_full = true;
        }
        if matches >= limit || output_full {
            break;
        }
    }
    let stopped = reason.map(|reason| {
        format!(
            "{reason}; {matches} match{} so far, narrow the path, glob or pattern",
            if matches == 1 { "" } else { "es" }
        )
    });
    let body = clip_chars(&output.join("\n"), limits.search_max_output_bytes).to_owned();
    let more = if (matches >= limit || output_full) && more_available {
        format!(
            "\n[more matches: call again with offset={}]",
            offset + matches
        )
    } else {
        String::new()
    };
    let text = if !output.is_empty() {
        let stopped_part = match &stopped {
            Some(stopped) => format!("\n[stopped: {stopped}]"),
            None => String::new(),
        };
        format!("{body}{stopped_part}{more}")
    } else if let Some(stopped) = &stopped {
        format!("No matches found ({stopped})")
    } else {
        "No matches found".to_owned()
    };
    (text, stopped.is_some(), matches)
}

/// `code_search` under a VM session. A directory target runs `io.search`
/// (the guest engine, bounded by the `SearchQuery` ceilings); a
/// single-file target is read through `io.read` and scanned locally with
/// the same regex and line rules as the host's file loop. Both feed one
/// output pipeline; paging (`offset`) is applied to the returned matches.
#[allow(clippy::too_many_arguments)] // mirrors the code_search tool's input surface
pub(crate) async fn code_search(
    env: &Arc<PackEnv>,
    ctx: &ToolContext,
    pattern: &str,
    path: &str,
    glob: Option<String>,
    literal: bool,
    ignore_case: bool,
    context: usize,
    limit: usize,
    offset: usize,
) -> Result<Outcome, CodingError> {
    let call = VmCall::new(env, ctx).expect("vm session present");
    throw_if_aborted(env, ctx)?;
    let limits = env.limits.clone();
    if pattern.chars().count() > limits.search_max_pattern_chars {
        return Err(CodingError::new("search_limit", "pattern is too long"));
    }
    let target = guest_path_of(env, path)?;
    let grant = call.grant(
        env,
        SandboxCapability::SandboxRead,
        vec![target.clone()],
        None,
    );
    // One cheap list probe decides directory vs single-file target,
    // exactly where the host calls `metadata`.
    let dir_probe = call
        .session
        .io
        .list(
            &grant,
            &target,
            ListOptions {
                max_entries: 1,
                include_hidden: true,
            },
        )
        .await;
    let (all, timed_out, guest_truncated, requested_max) = match dir_probe {
        Ok(_) => {
            let requested_max = (offset + limit).clamp(1, sandbox_limits::MAX_SEARCH_MATCHES);
            let query = SearchQuery {
                root: target.clone(),
                pattern: pattern.to_owned(),
                literal,
                ignore_case,
                glob: glob.clone(),
                context: context as u32,
                max_matches: requested_max as u32,
                max_entries: limits
                    .search_max_files
                    .min(sandbox_limits::MAX_SEARCH_ENTRIES) as u32,
                timeout_ms: limits.search_time_budget_ms,
            };
            let reply = call
                .session
                .io
                .search(&grant, query)
                .await
                .map_err(sandbox_coding_error_owned)?;
            if reply.timed_out && !reply.matches.is_empty() {
                // Partial scan: keep what came back, say so.
            }
            (
                reply.matches,
                reply.timed_out,
                reply.truncated,
                requested_max,
            )
        }
        Err(error)
            if matches!(
                error.code,
                SandboxErrorCode::NotFound
                    | SandboxErrorCode::PolicyDenied
                    | SandboxErrorCode::ArtifactInvalid
            ) =>
        {
            // Not a directory (or list-refused): treat as a single-file
            // target like the host's non-directory branch.
            let Ok(bytes) = vm_read_bytes(&call, env, &target, limits.search_max_file_bytes).await
            else {
                return Err(sandbox_coding_error(&error));
            };
            if bytes.contains(&0) {
                return Ok(no_matches_outcome(pattern));
            }
            if let Some(glob) = &glob {
                let compiled = compile_glob(glob)?;
                if !glob_matches(&compiled, &guest_relative(&target)) {
                    return Ok(no_matches_outcome(pattern));
                }
            }
            let regex = build_search_regex(pattern, literal, ignore_case)?;
            let text = String::from_utf8_lossy(&bytes);
            let lines: Vec<&str> = text
                .split('\n')
                .map(|line| line.strip_suffix('\r').unwrap_or(line))
                .collect();
            let rel = guest_relative(&target);
            let mut all: Vec<SearchMatch> = Vec::new();
            for index in 0..lines.len() {
                if !regex.is_match(clip_chars(lines[index], 8192)) {
                    continue;
                }
                let from = index.saturating_sub(context);
                let to = std::cmp::min(lines.len() - 1, index + context);
                all.push(SearchMatch {
                    path: rel.clone(),
                    line: (index + 1) as u32,
                    text: lines[index].to_owned(),
                    context_before: lines[from..index].iter().map(|s| (*s).to_owned()).collect(),
                    context_after: lines[index + 1..=to]
                        .iter()
                        .map(|s| (*s).to_owned())
                        .collect(),
                });
                if all.len() >= offset + limit {
                    break;
                }
            }
            (all, false, false, offset + limit)
        }
        Err(error) => return Err(sandbox_coding_error(&error)),
    };
    let reason = if timed_out {
        Some(format!(
            "{}s of scanning elapsed",
            seconds(limits.search_time_budget_ms)
        ))
    } else if guest_truncated {
        Some(format!("match limit {requested_max} reached"))
    } else {
        None
    };
    let (text, truncated, matched) =
        format_search_output(&all, offset, limit, &limits, reason, guest_truncated);
    Ok(Outcome {
        ok: true,
        text,
        summary: Some(search_summary(pattern, matched)),
        truncated,
        warnings: vec![],
        details: None,
    })
}
/// `find_files` under a VM session: `guest_walk` is the directory engine
/// (the host `Walk`'s BFS, skip rules and budgets rendered over guest
/// `io.list` replies); the glob, stop reasons and output text match the
/// host tool exactly.
pub(crate) async fn find_files(
    env: &Arc<PackEnv>,
    ctx: &ToolContext,
    pattern: &str,
    path: &str,
    limit: usize,
) -> Result<Outcome, CodingError> {
    let call = VmCall::new(env, ctx).expect("vm session present");
    throw_if_aborted(env, ctx)?;
    let limits = env.limits.clone();
    if pattern.chars().count() > limits.search_max_pattern_chars {
        return Err(CodingError::new("search_limit", "pattern is too long"));
    }
    let glob = compile_glob(pattern)?;
    let root = guest_path_of(env, path)?;
    // The host wrapper's `metadata().is_dir()` gate, guest-side: anything
    // the guest will not list (absent or file) is the same
    // `not_a_directory` refusal.
    let probe_grant = call.grant(
        env,
        SandboxCapability::SandboxRead,
        vec![root.clone()],
        None,
    );
    call.session
        .io
        .list(
            &probe_grant,
            &root,
            ListOptions {
                max_entries: 1,
                include_hidden: true,
            },
        )
        .await
        .map_err(|_| CodingError::new("not_a_directory", format!("{path} is not a directory")))?;
    let deadline = (env.clock)()
        .saturating_add(i64::try_from(limits.search_time_budget_ms).unwrap_or(i64::MAX));
    let walk = guest_walk(&call, env, ctx, &root, false, None, usize::MAX, deadline).await?;
    let root_rel = guest_relative(&root);
    let mut paths: Vec<String> = Vec::new();
    let mut hit_limit = false;
    for entry in walk.entries {
        let rel_for_glob = if root_rel.is_empty() {
            entry.rel.as_str()
        } else {
            let Some(rest) = entry.rel.strip_prefix(&format!("{root_rel}/")) else {
                continue;
            };
            rest
        };
        if !glob_matches(&glob, rel_for_glob) {
            continue;
        }
        paths.push(entry.rel);
        if paths.len() >= limit {
            hit_limit = true;
            break;
        }
    }
    let reasons: Vec<String> = [
        hit_limit.then(|| format!("result limit {limit} reached")),
        walk.entries_exhausted
            .then(|| format!("{} entries scanned", limits.search_max_files)),
        walk.timed_out.then(|| {
            format!(
                "{}s of scanning elapsed",
                seconds(limits.search_time_budget_ms)
            )
        }),
    ]
    .into_iter()
    .flatten()
    .collect();
    let body = if paths.is_empty() {
        "No files matched".to_owned()
    } else {
        paths.join("\n")
    };
    Ok(Outcome {
        ok: true,
        text: if reasons.is_empty() {
            body
        } else {
            format!("{body}\n[stopped: {}]", reasons.join("; "))
        },
        summary: Some(format!("{} file(s) matched {pattern}", paths.len())),
        truncated: !reasons.is_empty(),
        warnings: vec![],
        details: None,
    })
}

/// `repo_map` under a VM session: manifests and tree come from guest
/// `io.list`/`io.read`; sections, ordering (segment compare), indentation
/// and stop notes reproduce the host renderer.
pub(crate) async fn repo_map(
    env: &Arc<PackEnv>,
    ctx: &ToolContext,
    path: &str,
    depth: usize,
) -> Result<Outcome, CodingError> {
    let call = VmCall::new(env, ctx).expect("vm session present");
    throw_if_aborted(env, ctx)?;
    let limits = env.limits.clone();
    let target = guest_path_of(env, path)?;
    let list_grant = call.grant(
        env,
        SandboxCapability::SandboxRead,
        vec![target.clone()],
        None,
    );
    let probe = call
        .session
        .io
        .list(
            &list_grant,
            &target,
            ListOptions {
                max_entries: 1,
                include_hidden: true,
            },
        )
        .await
        .map_err(|_| CodingError::new("not_a_directory", format!("{path} is not a directory")))?;
    let _ = probe;
    let deadline = (env.clock)()
        .saturating_add(i64::try_from(limits.search_time_budget_ms).unwrap_or(i64::MAX));
    let walk = guest_walk(
        &call,
        env,
        ctx,
        &target,
        true,
        Some(depth),
        limits.repo_map_max_entries + 1,
        deadline,
    )
    .await?;
    let more = walk.entries.len() > limits.repo_map_max_entries;
    let mut entries: Vec<(String, bool)> = walk
        .entries
        .into_iter()
        .take(limits.repo_map_max_entries)
        .map(|entry| {
            let root_rel = guest_relative(&target);
            let rel = if root_rel.is_empty() {
                entry.rel
            } else {
                entry
                    .rel
                    .strip_prefix(&format!("{root_rel}/"))
                    .unwrap_or(entry.rel.as_str())
                    .to_owned()
            };
            (rel, entry.is_directory)
        })
        .collect();
    entries.sort_by(|a, b| {
        let left: Vec<&str> = a.0.split('/').collect();
        let right: Vec<&str> = b.0.split('/').collect();
        compare_segments(&left, &right)
    });
    // Top-level names for the case-insensitive manifest probe, like the
    // host's `read_dir` snapshot.
    let present: Vec<String> = match call
        .session
        .io
        .list(
            &list_grant,
            &target,
            ListOptions {
                max_entries: sandbox_limits::MAX_LIST_ENTRIES as u32,
                include_hidden: true,
            },
        )
        .await
    {
        Ok(reply) => reply
            .entries
            .into_iter()
            .filter(|entry| entry.kind == EntryKind::File)
            .map(|entry| entry.name)
            .collect(),
        Err(_) => Vec::new(),
    };
    let mut sections: Vec<String> = Vec::new();
    for wanted in crate::search::MANIFESTS {
        let Some(name) = present
            .iter()
            .find(|entry| entry.to_lowercase() == wanted)
            .cloned()
        else {
            continue;
        };
        let Ok(candidate) = target.join(&name) else {
            continue;
        };
        let Ok(reply) = call
            .session
            .io
            .read(&list_grant, &candidate, None, Some(1024 * 1024))
            .await
        else {
            continue;
        };
        let Ok(buffer) = b64().decode(&reply.data_b64) else {
            continue;
        };
        if buffer.iter().take(4096).any(|byte| *byte == 0) {
            continue;
        }
        // Host `read_excerpt`: clip at the budget with the same notice.
        let head = &buffer[..std::cmp::min(buffer.len(), limits.repo_map_manifest_excerpt_bytes)];
        let mut text = String::from_utf8_lossy(head).into_owned();
        if buffer.len() > limits.repo_map_manifest_excerpt_bytes {
            text = format!(
                "{text}\n[... {} more bytes]",
                buffer.len() - limits.repo_map_manifest_excerpt_bytes
            );
        }
        sections.push(format!("== {name} ==\n{text}"));
    }
    let tree: Vec<String> = entries
        .iter()
        .map(|(rel, is_directory)| {
            let parts: Vec<&str> = rel.split('/').collect();
            format!(
                "{}{}{}",
                "  ".repeat(parts.len() - 1),
                parts[parts.len() - 1],
                if *is_directory { "/" } else { "" }
            )
        })
        .collect();
    let notes: Vec<String> = [
        more.then(|| {
            format!(
                "{} entries shown; raise depth or narrow path",
                limits.repo_map_max_entries
            )
        }),
        walk.entries_exhausted
            .then(|| format!("{} entries scanned", limits.search_max_files)),
        walk.timed_out.then(|| {
            format!(
                "{}s of scanning elapsed",
                seconds(limits.search_time_budget_ms)
            )
        }),
    ]
    .into_iter()
    .flatten()
    .collect();
    sections.push(format!(
        "== tree (depth {depth}) ==\n{}",
        if tree.is_empty() {
            "(empty)".to_owned()
        } else {
            tree.join("\n")
        }
    ));
    if !notes.is_empty() {
        sections.push(format!("[stopped: {}]", notes.join("; ")));
    }
    let relative = guest_relative(&target);
    Ok(Outcome {
        ok: true,
        text: sections.join("\n\n"),
        summary: Some(format!(
            "mapped {} ({} entries)",
            if relative.is_empty() { "." } else { &relative },
            entries.len()
        )),
        truncated: !notes.is_empty(),
        warnings: vec![],
        details: None,
    })
}

// ---------------------------------------------------------------------------
// git_inspect
// ---------------------------------------------------------------------------

/// The git tool's write commands under a VM session: refused, not
/// downgraded, not shelled out (S06/S07). Host-side git execution against
/// a vm-target workspace would bypass the grant gate; applying changes
/// stays a future approved export path.
pub(crate) fn refuse_git_write(command_text: &str) -> CodingError {
    CodingError::new(
        "sandbox_policy_denied",
        format!(
            "git \"{command_text}\" is not a read-only inspection command; under a VM sandbox \
             only status, diff, log and show run. Workspace changes go through write_file/\
             edit_file or an approved export flow, never host git."
        ),
    )
}

/// `git_inspect` under a VM session: the guest runs the fixed read-only
/// command (no pager, hooks and helpers neutralized guest-side, §8.4) and
/// the host renders the reply with the exact output shaping of the host
/// tool. The reply text is digest-checked before it is shown (S17).
pub(crate) async fn git_inspect(
    env: &Arc<PackEnv>,
    ctx: &ToolContext,
    command: crate::git_tool::GitCommand,
    rev: Option<String>,
    path: Option<String>,
    staged: Option<bool>,
    max_count: Option<i64>,
) -> Result<Outcome, CodingError> {
    let call = VmCall::new(env, ctx).expect("vm session present");
    throw_if_aborted(env, ctx)?;
    // The contract's fixed-argv inspect request has no staged axis, and
    // inventing one guest-side (a second exec) would bypass the typed
    // surface: refuse explicitly instead.
    if staged == Some(true) {
        return Err(CodingError::new(
            "sandbox_policy_denied",
            "git diff --staged is not expressible as a typed inspect request under a VM \
             sandbox; run the unstaged diff or use an approved export flow",
        ));
    }
    let query_command = match command {
        crate::git_tool::GitCommand::Status => GitCommand::Status,
        crate::git_tool::GitCommand::Diff => GitCommand::Diff,
        crate::git_tool::GitCommand::Log => GitCommand::Log,
        crate::git_tool::GitCommand::Show => GitCommand::Show,
    };
    // The path filter is taken literally (no globs), workspace-relative,
    // and validated through the same guest-path gate as every other tool.
    let path = match path {
        Some(raw) => Some(guest_relative(&guest_path_of(env, &raw)?)),
        None => None,
    };
    let repo = GuestPath::parse(WORKSPACE).expect("workspace root is a valid guest path");
    let path_filters = match path.as_deref() {
        Some(value) if value != "." && !value.is_empty() => vec![value.to_owned()],
        _ => Vec::new(),
    };
    let max_count = u32::try_from(max_count.unwrap_or(20)).unwrap_or(20);
    let grant = call.grant(
        env,
        SandboxCapability::SandboxRead,
        vec![repo.clone()],
        None,
    );
    let reply = call
        .session
        .io
        .git_inspect(
            &grant,
            GitInspectQuery {
                repo,
                command: query_command,
                rev,
                path_filters,
                max_count,
            },
        )
        .await
        .map_err(sandbox_coding_error_owned)?;
    if ctx.cancellation.is_cancelled() {
        return Err(CodingError::new("aborted", "the call was aborted"));
    }
    // Untrusted guest text: it must hash to the digest that came with it.
    if ArtifactDigest::of_bytes(reply.text.as_bytes()) != reply.sha256 {
        return Err(CodingError::new(
            "sandbox_artifact_invalid",
            "git inspect reply does not hash to its own digest",
        ));
    }
    if !reply.truncated && reply.exit_code != 0 {
        let shown = slice_utf16(
            reply.text.trim_matches(crate::edit_engine::js_trim_matches),
            2000,
        );
        return Ok(Outcome {
            ok: false,
            text: format!(
                "git_failed: {}",
                if shown.is_empty() {
                    format!("exit {}", reply.exit_code)
                } else {
                    shown
                }
            ),
            summary: Some(format!("git {} failed", command.as_str())),
            truncated: false,
            warnings: vec![],
            details: None,
        });
    }
    let over = reply.truncated || reply.text.len() > env.limits.git_max_output_bytes;
    let text = if over {
        format!(
            "{}\n[output truncated at {}; narrow with args.path or args.rev]",
            slice_utf16(&reply.text, env.limits.git_max_output_bytes),
            format_size(env.limits.git_max_output_bytes)
        )
    } else {
        reply.text.clone()
    };
    Ok(Outcome {
        ok: true,
        text: if text
            .trim_matches(crate::edit_engine::js_trim_matches)
            .is_empty()
        {
            "(no output)".to_owned()
        } else {
            text
        },
        summary: Some(format!("git {}", command.as_str())),
        truncated: over,
        warnings: vec![],
        details: None,
    })
}
// ---------------------------------------------------------------------------
// exec / command / test
// ---------------------------------------------------------------------------

/// Settled state of a guest execution, from its own event stream.
enum Terminal {
    Exited {
        exit_code: Option<i32>,
        signal: Option<i32>,
        timed_out: bool,
    },
    /// Independently observed process death (worker-side evidence):
    /// termination is settled but no exit status was reported — absence of
    /// an exit code is never success.
    TerminationObserved,
    /// Completion cannot be proven; ownership intent is retained (S14).
    Uncertain(String),
}

struct Pumped {
    /// stdout+stderr merged in event order (untrusted guest bytes).
    bytes: Vec<u8>,
    /// Total produced bytes, including what the guest said it dropped.
    total: usize,
    /// The guest itself capped the output (`truncated` event).
    guest_truncated: bool,
    terminal: Option<Terminal>,
    /// We requested cancellation because the call was aborted.
    aborted_by_us: bool,
    /// We requested cancellation because the wall deadline passed.
    timed_out_by_us: bool,
}

/// Drain `exec_events` from a cursor until a terminal event, the deadline,
/// or cancellation. Cursor discipline (S16): only strictly newer sequences
/// are consumed; duplicates are suppressed, a gap is a protocol violation
/// (never silently skipped — missing output is never treated as absent).
async fn pump_events(
    call: &VmCall<'_>,
    env: &PackEnv,
    ctx: &ToolContext,
    exec_grant: &WorkspaceGrant,
    execution_id: &ExecutionId,
    deadline: Instant,
) -> Result<Pumped, CodingError> {
    let mut pumped = Pumped {
        bytes: Vec::new(),
        total: 0,
        guest_truncated: false,
        terminal: None,
        aborted_by_us: false,
        timed_out_by_us: false,
    };
    let mut cursor: u64 = 0;
    loop {
        throw_if_aborted(env, ctx)?;
        let events = call
            .session
            .io
            .exec_events(exec_grant, execution_id, cursor)
            .await
            .map_err(sandbox_coding_error_owned)?;
        let mut progressed = false;
        for event in events {
            if event.execution_id != *execution_id {
                return Err(CodingError::new(
                    "sandbox_protocol_incompatible",
                    "exec event names a different execution",
                ));
            }
            if event.sequence <= cursor {
                // Duplicate delivery (reconnect/suppression): skip, the
                // cursor already covers it.
                continue;
            }
            if event.sequence > cursor + 1 {
                return Err(CodingError::new(
                    "sandbox_protocol_incompatible",
                    format!(
                        "exec event stream skipped sequence {}..{}",
                        cursor + 1,
                        event.sequence
                    ),
                ));
            }
            cursor = event.sequence;
            progressed = true;
            match event.kind {
                ExecEventKind::StdoutChunk { bytes_b64 }
                | ExecEventKind::StderrChunk { bytes_b64 } => {
                    let decoded = b64().decode(&bytes_b64).map_err(|_| {
                        CodingError::new(
                            "sandbox_artifact_invalid",
                            "exec output chunk is not base64",
                        )
                    })?;
                    pumped.total += decoded.len();
                    pumped.bytes.extend_from_slice(&decoded);
                }
                ExecEventKind::Truncated { bytes, .. } => {
                    pumped.guest_truncated = true;
                    pumped.total += usize::try_from(bytes).unwrap_or(usize::MAX);
                }
                ExecEventKind::Exited {
                    exit_code,
                    signal,
                    timed_out,
                    ..
                } => {
                    pumped.terminal = Some(Terminal::Exited {
                        exit_code,
                        signal,
                        timed_out,
                    });
                }
                ExecEventKind::TerminationObserved { .. } => {
                    if pumped.terminal.is_none() {
                        pumped.terminal = Some(Terminal::TerminationObserved);
                    }
                }
                ExecEventKind::Uncertain { reason } => {
                    if pumped.terminal.is_none() {
                        pumped.terminal = Some(Terminal::Uncertain(reason));
                    }
                }
                // Advisory lifecycle events: recorded order matters,
                // content does not change the outcome.
                ExecEventKind::Accepted { .. }
                | ExecEventKind::Spawned { .. }
                | ExecEventKind::ResourceUpdate { .. } => {}
            }
            if pumped.terminal.is_some() {
                return Ok(pumped);
            }
        }
        if Instant::now() >= deadline {
            return Ok(pumped);
        }
        if !progressed {
            tokio::time::sleep(EVENT_POLL).await;
        }
    }
}
/// Outcome of the `io.exec` admission itself. `Unknown` mirrors the host
/// driver's "handed to the driver" boundary: the request may have been
/// admitted before the transport failed, so effects are possible and the
/// intent survives.
enum ExecDispatch {
    Started(ExecutionHandle),
    Refused(SandboxError),
    Unknown(SandboxError),
}

/// `run_command` / `run_tests` under a VM session. Mirrors the host
/// command path's gates and shapes exactly — dispatch guard (the approval
/// decision gate), once-per-process startup recovery, the durable
/// pre-dispatch execution intent, and the exclusive workspace lease that
/// serializes writers — then hands the command to the guest via `io.exec`
/// and drains `io.exec_events` to a terminal state. Host-side identity
/// rekeying (canonicalize/env-drift) is deliberately not applied: there is
/// no host spawn whose inputs could drift; the frozen request digest
/// computed by the bridge is the execution-side rekey.
pub(crate) async fn run_shell(
    env: Arc<PackEnv>,
    ctx: ToolContext,
    request: ShellRequest,
) -> Result<Outcome, CodingError> {
    // Everything `run_shell_inner` can return as `Err` happened before the
    // request reached the guest: the exclusive lease, the guard, the
    // identity checks — exactly the host wrapper's "not handed to the
    // driver" set, rendered the same way. Anything that may have reached
    // the guest travels inside `Dispatched` and surfaces raw.
    let dispatched = match run_shell_inner(env.clone(), ctx.clone(), &request).await {
        Err(error) => {
            return Ok(command_not_started(&format!(
                "{}: {}",
                error.code, error.message,
            )));
        }
        Ok(dispatched) => dispatched,
    };
    let mut intent = dispatched.intent;
    let handle = match dispatched.dispatch {
        ExecDispatch::Started(handle) => handle,
        ExecDispatch::Refused(error) => {
            // Proven pre-admission rejection: no effect ever existed, so
            // the durable intent is retired (re-arm the guard, which was
            // disarmed at the dispatch boundary, then clear it).
            intent.armed = true;
            intent.clear().await;
            return Err(sandbox_coding_error(&error));
        }
        // Admission may have happened before the transport failed:
        // effects are possible, the intent stays, the sandbox code
        // surfaces (reconcile, never self-retry).
        ExecDispatch::Unknown(error) => return Err(sandbox_coding_error(&error)),
    };
    finish_exec(env, ctx, &request, handle, dispatched.lease, intent).await
}

struct Dispatched {
    dispatch: ExecDispatch,
    /// The disarmed pre-dispatch intent: retired on refusal, kept until a
    /// terminal event (or retained on uncertainty) by `finish_exec`.
    intent: IntentGuard,
    /// The exclusive workspace lease, held from admission to settlement so
    /// writers stay serialized across the command's lifetime (S09).
    lease: WorkspaceLease,
}

async fn run_shell_inner(
    env: Arc<PackEnv>,
    ctx: ToolContext,
    request: &ShellRequest,
) -> Result<Dispatched, CodingError> {
    if env.is_closed() {
        return Err(CodingError::new(
            "command_not_started",
            "coding pack is closed",
        ));
    }
    // The approval decision gate stays in force for VM sessions: the host
    // guard authorizes, the worker grant executes (AND-gate).
    if let Some(guard) = &ctx.dispatch_guard {
        guard
            .check()
            .await
            .map_err(|error| CodingError::new(&error.code, error.message))?;
    }
    let cwd = guest_path_of(&env, request.cwd.as_deref().unwrap_or("."))?;
    let tool_call_id = ctx
        .tool_call_id
        .as_deref()
        .and_then(|value| ToolCallId::new(value).ok())
        .ok_or_else(|| {
            CodingError::new(
                "command_not_started",
                "a sandboxed command requires the model tool-call id",
            )
        })?;
    // Minted BEFORE the lease, exactly like the host path (ADR 0020): the
    // same identity names the durable record and the guest execution. The
    // wire id uses `-` because the contract id grammar admits no `:`; the
    // durable record uses the identical string, so cleanup evidence names
    // the exact execution.
    let execution_id = format!("{}-{}", process_id(), uuid::Uuid::new_v4().simple());
    let exec_id = ExecutionId::new(execution_id.clone())
        .map_err(|_| CodingError::new("command_not_started", "execution identity is invalid"))?;
    startup_recovery().await;
    let holder = LeaseHolder {
        chat_id: ctx.chat_id.clone(),
        run_id: ctx.run_id.clone(),
        tool_call_id: ctx.tool_call_id.clone(),
        tool: request.tool.to_owned(),
        label: slice_utf16(&request.command, 200),
    };
    let mut intent = IntentGuard {
        coordinator: Arc::clone(&env.coordinator),
        root: env.workspace_root.clone(),
        execution_id: execution_id.clone(),
        armed: true,
    };
    // Durable pre-dispatch intent, written before the lease is admitted
    // (B5). There is no host pgid for a guest process; `spawned` is set
    // when the guest reports its own pid, and startup reconciliation
    // treats a record without `spawned` as possibly-not-started — never
    // host cleanup of a process the host never owned.
    if let Err(error) = env
        .coordinator
        .record_execution_intent(&ExecutionIntent {
            root: env.workspace_root.clone(),
            identity: crate::coordination::CommandCleanupIdentity {
                execution_id: execution_id.clone(),
                owner_pid: i64::from(process_id()),
                pgid: None,
                spawned_at_ms: now_ms_u64(),
            },
            holder: holder.clone(),
            spawned: false,
        })
        .await
    {
        intent.clear().await;
        return Err(error);
    }
    // Every command may write, so every command takes the exclusive lease,
    // with the same busy/closed/cancelled mapping as the host path.
    let acquiring = env.coordinator.acquire_workspace(AcquireWorkspaceInput {
        root: env.workspace_root.clone(),
        holder: holder.clone(),
        cancellation: Some(ctx.cancellation.clone()),
        max_wait_ms: Some(env.coordination_wait_ms),
        execution_id: Some(execution_id.clone()),
    });
    let lease = match tokio::select! { biased;
        _ = env.closed() => Err(CodingError::new("cancelled", "coding pack closed before command dispatch")),
        result = acquiring => result,
    } {
        Ok(lease) => lease,
        Err(error) => {
            intent.clear().await;
            return Err(error);
        }
    };
    // Fresh guard + liveness check immediately before admission, mirroring
    // the host authority's double-check at the spawn boundary.
    if let Some(guard) = &ctx.dispatch_guard {
        guard
            .check()
            .await
            .map_err(|error| CodingError::new(&error.code, error.message))?;
    }
    throw_if_aborted(&env, &ctx)?;
    let mut environment = GuestEnvironment::new();
    // Only the pack's explicit environment reaches the guest: a sandboxed
    // command never sees the host process environment (S17); credential-
    // shaped names are refused by the contract's own gate.
    if let Some(entries) = env.environment.as_deref() {
        let mut names: Vec<&String> = entries.keys().collect();
        names.sort();
        for name in names {
            if let Err(error) = environment.insert(name, &entries[name]) {
                return Err(CodingError::new(
                    "sandbox_policy_denied",
                    format!("command environment entry {name} is not guest-safe: {error}"),
                ));
            }
        }
    }
    let call = VmCall::new(&env, &ctx).expect("vm session present");
    let exec_grant = call.grant(
        &env,
        SandboxCapability::ProcessExec,
        vec![cwd.clone()],
        Some(cwd.clone()),
    );
    let exec_request = ExecRequest {
        execution_id: exec_id.clone(),
        tool_call_id,
        // Zero form: the `SandboxWorkspaceIo` implementation fills the
        // real digest (single authority, `request_digest_of`) immediately
        // before dispatch. The pack never computes it (S07).
        request_digest: RequestDigest::of_canonical_json(b""),
        argv: ExecArgv::Shell {
            command: request.command.clone(),
        },
        cwd: cwd.clone(),
        environment,
        timeout_sec: u32::try_from(request.timeout_sec.max(1)).unwrap_or(u32::MAX),
        max_output_bytes: env.limits.command_max_log_bytes,
        max_processes: agentkit_sandbox_contracts::limits::DEFAULT_EPHEMERAL_MAX_PROCESSES,
    };
    // From here the request may have side effects: the intent must
    // survive even if this future is dropped at an await (B1/Drop).
    intent.disarm();
    // The lease stays held through admission and the whole event pump
    // (taken out of `Dispatched` by `run_shell`): writers serialize across
    // the command's lifetime (S09). A harness drop of this future releases
    // it, matching the host awaitable path; guest tree teardown is the
    // guest supervisor's job, so no host cleanup outlives the call.
    let dispatch = match env
        .vm
        .as_ref()
        .expect("vm session present")
        .io
        .exec(&exec_grant, exec_request)
        .await
    {
        Ok(handle) => ExecDispatch::Started(handle),
        Err(error) => {
            // A refusal code proves pre-admission rejection; the
            // uncertain codes mean admission may have happened before
            // the transport failed.
            if matches!(
                error.code,
                SandboxErrorCode::Unavailable
                    | SandboxErrorCode::ExecutionUncertain
                    | SandboxErrorCode::TerminationUnverified
            ) {
                ExecDispatch::Unknown(error)
            } else {
                ExecDispatch::Refused(error)
            }
        }
    };
    Ok(Dispatched {
        dispatch,
        intent,
        lease,
    })
}
/// Drive a dispatched execution to settlement: pump events, request a
/// cancel at the wall deadline or on abort, then render the result with
/// the host command pipeline's own truncation, footer and summary.
async fn finish_exec(
    env: Arc<PackEnv>,
    ctx: ToolContext,
    request: &ShellRequest,
    handle: ExecutionHandle,
    lease: WorkspaceLease,
    mut intent: IntentGuard,
) -> Result<Outcome, CodingError> {
    let call = VmCall::new(&env, &ctx).expect("vm session present");
    let cwd = guest_path_of(&env, request.cwd.as_deref().unwrap_or("."))?;
    let exec_grant = call.grant(
        &env,
        SandboxCapability::ProcessExec,
        vec![cwd.clone()],
        Some(cwd.clone()),
    );
    let execution_id = handle.execution_id.clone();
    let timeout = Duration::from_secs(request.timeout_sec.max(1));
    let mut pumped = pump_events(
        &call,
        &env,
        &ctx,
        &exec_grant,
        &execution_id,
        Instant::now() + timeout + DISPATCH_GRACE,
    )
    .await?;
    let mut cancel_requested = false;
    if pumped.terminal.is_none() {
        // The guest supervisor enforces `timeout_sec` itself; reaching this
        // line means it did not report one — either the wall clock won or
        // the call was aborted. Ask for cancellation, then listen once
        // more. A cancel is only ever REQUESTED (S14).
        cancel_requested = true;
        let aborted = ctx.cancellation.is_cancelled() || env.is_closed();
        let state = call
            .session
            .io
            .exec_cancel(&exec_grant, &execution_id)
            .await
            .map_err(sandbox_coding_error_owned)?;
        pumped = pump_events(
            &call,
            &env,
            &ctx,
            &exec_grant,
            &execution_id,
            Instant::now() + CANCEL_GRACE,
        )
        .await?;
        if pumped.terminal.is_none() {
            // Terminal state unproven after cancel: the command's effects
            // are unknown. Retain the ownership intent (S14) and surface
            // the uncertainty; reconcile is the host/worker's job, never a
            // pack-side retry.
            return Err(CodingError::new(
                SandboxErrorCode::ExecutionUncertain.as_str(),
                format!(
                    "sandbox_execution_uncertain: command {} was cancelled{} but reached no \
                     proven terminal state; the execution intent is retained for reconciliation \
                     (retry class: {})",
                    slice_utf16(&request.command, 120),
                    if state.uncertain {
                        " (worker also reports the cancel as uncertain)"
                    } else {
                        ""
                    },
                    retry_text(SandboxErrorCode::ExecutionUncertain.retry_class())
                ),
            ));
        }
        // The settle path below attributes the termination; a cancelled
        // command never counts as success.
        if aborted && let Some(Terminal::Exited { timed_out, .. }) = &mut pumped.terminal {
            *timed_out = false;
        }
        // Remember why we cancelled so the footer/summary match the host:
        // wall-clock overruns read as timeouts, aborts as aborted.
        if aborted {
            pumped.aborted_by_us = true;
        } else {
            pumped.timed_out_by_us = true;
        }
    }
    // Lease ends with settlement (or retained-intent uncertainty below);
    // guest tree teardown belongs to the guest supervisor.
    drop(lease);
    let (guest_exit, guest_signal, guest_timed_out, observed) = match &pumped.terminal {
        Some(Terminal::Exited {
            exit_code,
            signal,
            timed_out,
        }) => (*exit_code, *signal, *timed_out, "guest-events".to_owned()),
        Some(Terminal::TerminationObserved) => (None, None, false, "worker-observed".to_owned()),
        Some(Terminal::Uncertain(reason)) => {
            // The guest itself says completion is unprovable: same
            // discipline as the cancel path — intent retained, error
            // surfaced, never a silent success.
            return Err(CodingError::new(
                SandboxErrorCode::ExecutionUncertain.as_str(),
                format!(
                    "sandbox_execution_uncertain: {} (retry class: {}); the execution intent \
                     is retained for reconciliation",
                    reason,
                    retry_text(SandboxErrorCode::ExecutionUncertain.retry_class())
                ),
            ));
        }
        None => unreachable!("terminal state was established above"),
    };
    // Settled: the ownership record has served its purpose (the guest
    // supervisor owns the process tree; there is no host group to scan).
    let _ = env
        .coordinator
        .clear_execution_intent(&intent.root, &intent.execution_id)
        .await;
    intent.armed = true;
    intent.clear().await;
    let signal = guest_signal.map(guest_signal_label);
    let status = command_status(
        guest_timed_out || pumped.timed_out_by_us,
        pumped.aborted_by_us,
        &signal,
        guest_exit,
        request.timeout_sec,
    );
    let text = String::from_utf8_lossy(&pumped.bytes).into_owned();
    let cut = truncate_tail(
        &text,
        TruncationLimits {
            max_lines: env.limits.command_tail_max_lines,
            max_bytes: env.limits.command_tail_max_bytes,
        },
    );
    let mut notice = String::new();
    if cut.truncated {
        notice = describe_truncation(
            std::cmp::max(pumped.total, cut.total_bytes),
            cut.output_lines,
            cut.output_bytes,
            None,
        );
    } else if pumped.guest_truncated {
        notice = format!(
            "[Output truncated: the guest capped the stream at {} of {} sent to the event \
             buffer.]",
            format_size(cut.total_bytes),
            format_size(pumped.total)
        );
    }
    let parts = [
        cut.content.trim_end_matches('\n').to_owned(),
        notice,
        status.footer,
    ]
    .into_iter()
    .filter(|part| !part.is_empty())
    .collect::<Vec<_>>();
    let mut details = Map::new();
    details.insert("exitCode".to_owned(), json!(guest_exit));
    details.insert("signal".to_owned(), json!(signal));
    details.insert(
        "timedOut".to_owned(),
        json!(guest_timed_out || pumped.timed_out_by_us),
    );
    details.insert(
        "teardown".to_owned(),
        json!({
            "observed": observed,
            "replayed": handle.replayed,
            "cancelRequested": cancel_requested,
        }),
    );
    Ok(Outcome {
        ok: status.ok,
        text: parts.join("\n"),
        summary: Some(status.summary),
        truncated: cut.truncated || pumped.guest_truncated,
        // No host sandbox-exec is involved; the command ran inside the VM,
        // and `details.teardown.observed` states who proved termination.
        warnings: vec![],
        details: Some(Value::Object(details)),
    })
}
