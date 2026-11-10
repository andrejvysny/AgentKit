//! The two isolated Chat launch profiles and the frozen configuration
//! identity (AGENTKIT-109, AGENTKIT-107 §4).
//!
//! Public CLI surface only: every flag below is part of the documented
//! official interface, and the driver never uses the private stdio control
//! protocol. Isolation invariants (both profiles):
//!
//! * `--setting-sources ""` loads no user/project/local settings files, so
//!   ambient settings cannot widen the tool surface; managed settings still
//!   apply and are respected (detected at preflight, never bypassed).
//! * `--strict-mcp-config` + `--mcp-config <private file>` pins the MCP
//!   surface exactly: plain Chat gets zero servers; owned-MCP Chat gets
//!   exactly the one task-scoped AgentKit broker.
//! * `--tools ""` disables every built-in tool namespace; plain Chat also
//!   denies the `mcp__*` namespace. Permission prompts are structurally
//!   disabled (`--permission-prompts none`, qualified floor 2.1.259), so an
//!   unattended run can never hang on an invisible prompt.
//! * Prompts travel as typed argv plus a private per-run file, never inside
//!   a shell command string (AGENTKIT-110).
//! * Additive environment only; the supervisor additionally scrubs the
//!   ambient interrupted-turn resume variable (AGENTKIT-116).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{json, value::Value};
use uuid::Uuid;

use agentkit_native::{
    CapabilityState, LaunchPlan, McpEndpointSpec, NativeExecutionTarget, NativeProfile,
    NativeRefusal, PermissionPromptPolicy, TurnInput, TurnResume, canonical_digest,
};

/// Prefix for private per-run directories (AGENTKIT-109).
pub const PRIVATE_RUN_DIR_PREFIX: &str = "agentkit-claude-run";
/// Backend identity this launch layout is qualified for.
pub const BACKEND_ID: &str = "claude-code";

/// Default vendor-internal turn bounds per profile (AGENTKIT-116 bounds;
/// `TurnInput.max_vendor_turns` overrides when present).
pub struct ChatLaunchProfiles;

impl ChatLaunchProfiles {
    /// Plain Chat: exactly one vendor agentic turn per admitted input.
    pub const PLAIN_MAX_TURNS: u32 = 1;
    /// Owned-MCP Chat: the tool loop may interleave several vendor turns;
    /// bounded to keep a runaway loop finite.
    pub const MCP_MAX_TURNS: u32 = 30;
}

/// Vendor-side HTTP MCP request budget (`MCP_TOOL_TIMEOUT`, ms) for the
/// owned-broker profile. MUST exceed the WHOLE compatible deadline chain
/// below it (review R5): the broker's human approval gate
/// (`BROKER_AUTHORIZE_BUDGET_MS`, 360 000 ms) PLUS the effect timeout
/// (`BrokerLimits::default().call_timeout_ms`, 120 000 ms) plus a 60 s
/// margin — so the broker's own bounds, not the vendor transport's
/// default, are the first to end a slow governed call, and an approval
/// answered at the far edge of its advertised window can still execute.
pub const BROKER_MCP_REQUEST_BUDGET_MS: i64 = 540_000;

/// Build the frozen launch plan for one attempt.
///
/// `per_run_dir` must already exist (created by the caller with 0700);
/// [`write_per_run_files`] stages its files. The plan's `cwd` is the stable
/// sibling `<per_run_dir.parent()>/cwd` shared by all attempts (the caller
/// creates it; it is never removed by per-attempt cleanup). The plan is deterministic for a
/// frozen (target, input, mcp) triple except the freshly assigned session id
/// for `Fresh`, which is returned in `native_session_id` so the host records
/// it before spawn (AGENTKIT-113 lineage).
pub fn build_launch_plan(
    target: &NativeExecutionTarget,
    input: &TurnInput,
    mcp: Option<&McpEndpointSpec>,
    per_run_dir: PathBuf,
) -> Result<LaunchPlan, NativeRefusal> {
    validate_shape(target, input, mcp)?;

    let exe = target.executable.clone();
    let mut argv: Vec<String> = vec![
        exe,
        "-p".into(),
        "--output-format".into(),
        "stream-json".into(),
        "--input-format".into(),
        "stream-json".into(),
        "--verbose".into(),
        "--include-partial-messages".into(),
        "--no-chrome".into(),
        "--disable-slash-commands".into(),
        // Isolation: load no user/project/local settings files.
        "--setting-sources".into(),
        String::new(),
        // Isolation: the MCP surface is exactly what we pin below.
        "--strict-mcp-config".into(),
        "--mcp-config".into(),
        per_run_dir.join("mcp.json").display().to_string(),
        // Isolation: no built-in tool namespaces in either profile.
        "--tools".into(),
        String::new(),
        // Unattended: nothing may raise a permission prompt.
        "--permission-prompts".into(),
        "none".into(),
        // Our own pinned settings file.
        "--settings".into(),
        per_run_dir.join("settings.json").display().to_string(),
    ];

    let (permission_policy, max_vendor_turns) = match target.profile {
        NativeProfile::PlainChat => {
            argv.push("--safe-mode".into());
            argv.push("--disallowedTools".into());
            argv.push("mcp__*".into());
            (
                PermissionPromptPolicy::None,
                input
                    .max_vendor_turns
                    .unwrap_or(ChatLaunchProfiles::PLAIN_MAX_TURNS),
            )
        }
        NativeProfile::OwnedMcpChat => {
            let spec = mcp.expect("validated: OwnedMcpChat carries an endpoint");
            argv.push("--allowedTools".into());
            argv.push(spec.broker_tool_names.join(","));
            (
                PermissionPromptPolicy::PreapprovedBroker {
                    broker_tool_names: spec.broker_tool_names.clone(),
                },
                input
                    .max_vendor_turns
                    .unwrap_or(ChatLaunchProfiles::MCP_MAX_TURNS),
            )
        }
    };

    if let Some(model) = target
        .requested_model
        .as_deref()
        .filter(|m| !m.trim().is_empty())
    {
        argv.push("--model".into());
        argv.push(model.to_owned());
    }
    if let Some(effort) = target
        .requested_effort
        .as_deref()
        .filter(|e| !e.trim().is_empty())
    {
        argv.push("--effort".into());
        argv.push(effort.to_owned());
    }

    let native_session_id = match &input.resume {
        TurnResume::Fresh { .. } => {
            // The CLI validates `--session-id` as a canonical hyphenated
            // UUID (8-4-4-4-12); the 32-hex `simple()` form is rejected
            // with "Invalid session ID" and exit 1.
            let id = Uuid::new_v4().hyphenated().to_string();
            argv.push("--session-id".into());
            argv.push(id.clone());
            Some(id)
        }
        TurnResume::Exact { vendor_session_id } => {
            argv.push("--resume".into());
            argv.push(vendor_session_id.clone());
            Some(vendor_session_id.clone())
        }
        TurnResume::Fork {
            vendor_session_id, ..
        } => {
            argv.push("--resume".into());
            argv.push(vendor_session_id.clone());
            argv.push("--fork-session".into());
            // The forked session id is vendor-assigned and observed from
            // the init frame; nothing is preassigned.
            None
        }
    };

    if input.system_prompt.is_some() {
        argv.push("--system-prompt-file".into());
        argv.push(per_run_dir.join("prompt.txt").display().to_string());
    }

    argv.push("--max-turns".into());
    argv.push(max_vendor_turns.to_string());

    // Additive environment only; never credential material. Background work
    // and nonessential traffic are pinned off for unattended Chat.
    let mut env = vec![
        (
            "CLAUDE_CODE_DISABLE_BACKGROUND_TASKS".into(),
            "1".to_owned(),
        ),
        (
            "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC".into(),
            "1".to_owned(),
        ),
    ];
    if mcp.is_some() {
        // One coherent deadline model (review §2.9): the vendor's HTTP MCP
        // request default is 60s, which would kill a broker call — and its
        // human approval wait — long before the broker's own 120s call
        // budget or the host idle budget. Pin the vendor-side request
        // budget ABOVE the broker call timeout (BrokerLimits default
        // 120 000 ms) so the broker, not the transport, is the first to
        // decide a slow governed call.
        env.push((
            "MCP_TOOL_TIMEOUT".into(),
            BROKER_MCP_REQUEST_BUDGET_MS.to_string(),
        ));
    }

    let fingerprint = config_identity(target, &argv, &env);

    // The working directory is STABLE across attempts: the CLI keys its
    // session transcript store (`~/.claude/projects/<encoded cwd>/`) by
    // cwd, so a fresh random cwd per attempt would leak a project dir per
    // session and break exact resume. It is the sibling `<run_root>/cwd`
    // of the unique per-attempt dir (which stays private and is the only
    // thing cleanup removes).
    let cwd = per_run_dir
        .parent()
        .map(|root| root.join("cwd"))
        .unwrap_or_else(|| per_run_dir.clone());

    Ok(LaunchPlan {
        argv,
        env,
        cwd,
        per_run_dir,
        permission_policy,
        max_vendor_turns: Some(max_vendor_turns),
        native_session_id,
        fingerprint,
    })
}

/// Stage the private per-run files (0600) the argv references: pinned MCP
/// config, pinned settings, and the frozen application system prompt when
/// one is in force. No user credential material is ever written; the MCP
/// config by design carries the single-use, attempt-scoped broker bearer
/// token, which is secret to this attempt and dies with it (revocation on
/// every terminal path).
pub fn write_per_run_files(
    per_run_dir: &Path,
    system_prompt: Option<&str>,
    mcp: Option<&McpEndpointSpec>,
) -> Result<(), NativeRefusal> {
    let write_private = |name: &str, bytes: &[u8]| -> Result<(), NativeRefusal> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let path = per_run_dir.join(name);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
            .map_err(|e| {
                NativeRefusal::new(
                    "per_run_write",
                    format!("cannot stage {}: {e}", path.display()),
                    true,
                )
            })?;
        file.write_all(bytes).map_err(|e| {
            NativeRefusal::new(
                "per_run_write",
                format!("cannot write {}: {e}", path.display()),
                true,
            )
        })?;
        file.flush().map_err(|e| {
            NativeRefusal::new(
                "per_run_write",
                format!("cannot flush {}: {e}", path.display()),
                true,
            )
        })
    };

    write_private("mcp.json", mcp_config_json(mcp)?.as_bytes())?;
    write_private("settings.json", b"{}\n")?;
    match system_prompt {
        Some(prompt) => write_private("prompt.txt", prompt.as_bytes())?,
        None => {
            // A reused per-run directory must not keep a stale prompt the
            // argv no longer references.
            let stale = per_run_dir.join("prompt.txt");
            match std::fs::remove_file(&stale) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(NativeRefusal::new(
                        "per_run_write",
                        format!("cannot remove stale {}: {e}", stale.display()),
                        true,
                    ));
                }
            }
        }
    }
    Ok(())
}

/// The pinned `--mcp-config` document: zero servers for plain Chat, exactly
/// the one scoped broker for owned-MCP Chat (AGENTKIT-114 endpoint shapes).
pub fn mcp_config_json(mcp: Option<&McpEndpointSpec>) -> Result<String, NativeRefusal> {
    let servers = match mcp {
        None => json!({}),
        Some(spec) => {
            let entry = endpoint_value(spec)?;
            let mut map = serde_json::Map::new();
            map.insert(spec.server_name.clone(), entry);
            Value::Object(map)
        }
    };
    serde_json::to_string_pretty(&json! { {"mcpServers": servers} }).map_err(|e| {
        NativeRefusal::new(
            "mcp_endpoint_shape",
            format!("cannot encode MCP config: {e}"),
            false,
        )
    })
}

/// Frozen configuration identity: argv template (paths included — no
/// secrets), environment keys, profile and prompt revision. The layout is
/// part of the admission snapshot (AGENTKIT-107 §3.4): a later dispatch
/// must match or be re-admitted, never reinterpreted.
pub fn config_identity(
    target: &NativeExecutionTarget,
    argv: &[String],
    env: &[(String, String)],
) -> String {
    let mut keys: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
    keys.sort_unstable();
    canonical_digest(&json!({
        "backend": BACKEND_ID,
        "profile": target.profile,
        "argvTemplate": argv,
        "envKeys": keys,
        "systemPromptRevision": target.system_prompt_revision,
    }))
}

/// Stable per-attempt private run directory path under `run_root`
/// (AGENTKIT-109: one dir per attempt, removed at cleanup).
pub fn new_per_run_dir(run_root: &Path) -> PathBuf {
    run_root.join(format!(
        "{PRIVATE_RUN_DIR_PREFIX}-{}",
        Uuid::new_v4().simple()
    ))
}

fn endpoint_value(spec: &McpEndpointSpec) -> Result<Value, NativeRefusal> {
    match (&spec.stdio_command, &spec.http_url) {
        (Some(cmd), None) if !cmd.is_empty() => {
            let mut entry = serde_json::Map::new();
            entry.insert("type".into(), json!("stdio"));
            entry.insert("command".into(), json!(cmd[0]));
            entry.insert("args".into(), json!(&cmd[1..]));
            entry.insert("env".into(), json!({}));
            Ok(Value::Object(entry))
        }
        (None, Some(url)) if !url.trim().is_empty() => {
            let mut entry = serde_json::Map::new();
            entry.insert("type".into(), json!("http"));
            entry.insert("url".into(), json!(url));
            if let Some(token) = spec.auth_token.as_deref().filter(|t| !t.trim().is_empty()) {
                entry.insert(
                    "headers".into(),
                    json!({ "Authorization": format!("Bearer {token}") }),
                );
            }
            Ok(Value::Object(entry))
        }
        _ => Err(NativeRefusal::new(
            "mcp_endpoint_shape",
            "MCP endpoint needs exactly one of stdioCommand or httpUrl",
            false,
        )),
    }
}

fn validate_shape(
    target: &NativeExecutionTarget,
    input: &TurnInput,
    mcp: Option<&McpEndpointSpec>,
) -> Result<(), NativeRefusal> {
    if target.backend != BACKEND_ID {
        return Err(NativeRefusal::new(
            "backend_mismatch",
            format!(
                "driver is for '{BACKEND_ID}', target says '{}'",
                target.backend
            ),
            false,
        ));
    }
    if input.text.trim().is_empty() && input.context_blocks.is_empty() {
        return Err(NativeRefusal::new(
            "empty_input",
            "admitted input carries neither text nor context",
            false,
        ));
    }
    match target.profile {
        NativeProfile::PlainChat => {
            if mcp.is_some() {
                return Err(NativeRefusal::new(
                    "mcp_on_plain_profile",
                    "plain Chat must not receive an MCP endpoint",
                    false,
                ));
            }
            require_supported(target, "plain-chat")?;
        }
        NativeProfile::OwnedMcpChat => {
            let spec = mcp.ok_or_else(|| {
                NativeRefusal::new(
                    "mcp_endpoint_missing",
                    "owned-MCP Chat requires the scoped broker endpoint",
                    false,
                )
            })?;
            if spec.server_name.trim().is_empty() || spec.broker_tool_names.is_empty() {
                return Err(NativeRefusal::new(
                    "mcp_endpoint_shape",
                    "broker endpoint needs a server name and exact tool names",
                    false,
                ));
            }
            endpoint_value(spec)?;
            require_supported(target, "owned-mcp-chat")?;
            require_supported(target, "strict-mcp-config")?;
        }
    }
    match &input.resume {
        TurnResume::Exact { vendor_session_id }
        | TurnResume::Fork {
            vendor_session_id, ..
        } => {
            if vendor_session_id.trim().is_empty() {
                return Err(NativeRefusal::new(
                    "session_identity_missing",
                    "exact resume requires the bound vendor session id",
                    false,
                ));
            }
        }
        TurnResume::Fresh { .. } => {}
    }
    if matches!(input.resume, TurnResume::Fork { .. }) {
        // Fork stays unqualified in this delivery (AGENTKIT-107 §5): the
        // host must choose Fresh seeding instead. Explicit refusal, never a
        // silent downgrade.
        return Err(NativeRefusal::new(
            "vendor_fork_unqualified",
            "session forking is not qualified; use Fresh with seeded ancestry",
            false,
        ));
    }
    Ok(())
}

fn require_supported(target: &NativeExecutionTarget, flag: &str) -> Result<(), NativeRefusal> {
    match target
        .capabilities
        .flags
        .get(flag)
        .cloned()
        .unwrap_or(CapabilityState::Unknown)
    {
        CapabilityState::Supported { .. } => Ok(()),
        // Version floor met: admitted, but honestly unqualified (review §3).
        CapabilityState::Capable { .. } => Ok(()),
        CapabilityState::Unsupported { reason } => Err(NativeRefusal::new(
            "capability_unsupported",
            format!("{flag} is unsupported: {reason}"),
            false,
        )),
        CapabilityState::Unknown => Err(NativeRefusal::new(
            "capability_unknown",
            format!("{flag} is unknown for version {:?}", target.backend_version),
            true,
        )),
    }
}

/// Convenience used by the backend and the conformance harness: the argv
/// minus the executable (layout comparison without a probed path).
pub fn argv_template(argv: &[String]) -> Vec<String> {
    argv.iter().skip(1).cloned().collect()
}

/// Flag set of an argv (everything starting with `--`/`-p`), for admission
/// comparison and diagnostics.
pub fn flag_set(argv: &[String]) -> BTreeMap<String, Option<String>> {
    let mut out = BTreeMap::new();
    let mut i = 0;
    while i < argv.len() {
        let a = &argv[i];
        if a.starts_with("--") || a == "-p" {
            let value = argv.get(i + 1).filter(|n| !n.starts_with("--")).cloned();
            let has_value = value.is_some();
            out.insert(a.clone(), value);
            if has_value {
                i += 1;
            }
        }
        i += 1;
    }
    out
}

#[cfg(test)]
pub(crate) mod tests_exports {
    //! Fixtures shared with the backend module's tests (same crate).
    use super::*;
    use agentkit_native::{
        AuthExpectation, CapabilitySnapshot, ContentPolicy, EXECUTION_TARGET_SCHEMA_VERSION,
    };

    pub(crate) fn snap(flags: &[&str]) -> CapabilitySnapshot {
        let mut flags_map = std::collections::BTreeMap::new();
        for f in [
            "plain-chat",
            "owned-mcp-chat",
            "strict-mcp-config",
            "safe-mode",
            "permission-prompts-none",
        ] {
            flags_map.insert(
                f.to_string(),
                CapabilityState::Unsupported {
                    reason: "test default".into(),
                },
            );
        }
        for f in flags {
            if let Some(name) = f.strip_prefix('~') {
                flags_map.insert(name.to_string(), CapabilityState::Unknown);
                continue;
            }
            flags_map.insert(
                (*f).to_string(),
                CapabilityState::Supported {
                    qualified_version: "2.1.310".into(),
                    evidence: "test".into(),
                },
            );
        }
        CapabilitySnapshot {
            backend: BACKEND_ID.into(),
            matrix_version: 1,
            qualified_version: "2.1.310".into(),
            flags: flags_map,
        }
    }

    pub const MCP_TARGET_FLAGS: &[&str] = &[
        "plain-chat",
        "owned-mcp-chat",
        "strict-mcp-config",
        "safe-mode",
        "permission-prompts-none",
    ];

    pub fn plain_target(executable: &str) -> NativeExecutionTarget {
        NativeExecutionTarget {
            schema_version: EXECUTION_TARGET_SCHEMA_VERSION,
            backend: BACKEND_ID.into(),
            backend_version: Some("2.1.310".into()),
            profile: NativeProfile::PlainChat,
            executable: executable.into(),
            requested_model: Some("sonnet".into()),
            requested_effort: None,
            auth_expectation: AuthExpectation::SubscriptionLogin,
            capabilities: snap(MCP_TARGET_FLAGS),
            config_identity: "cfg".into(),
            system_prompt_revision: Some("rev-7".into()),
            content: ContentPolicy::default(),
        }
    }

    pub fn turn_input(resume: TurnResume) -> TurnInput {
        TurnInput {
            input_id: "in-1".into(),
            text: "hello".into(),
            context_blocks: vec![],
            resume,
            system_prompt_revision: Some("rev-7".into()),
            system_prompt: Some("Stable app prompt.".into()),
            max_vendor_turns: None,
            turn_deadline_ms: 60_000,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentkit_native::{
        AuthExpectation, CapabilitySnapshot, ContentPolicy, EXECUTION_TARGET_SCHEMA_VERSION,
    };

    fn snap(flags: &[&str]) -> CapabilitySnapshot {
        super::tests_exports::snap(flags)
    }

    fn target(profile: NativeProfile, flags: &[&str]) -> NativeExecutionTarget {
        NativeExecutionTarget {
            schema_version: EXECUTION_TARGET_SCHEMA_VERSION,
            backend: BACKEND_ID.into(),
            backend_version: Some("2.1.310".into()),
            profile,
            executable: "/usr/local/bin/claude".into(),
            requested_model: Some("sonnet".into()),
            requested_effort: Some("high".into()),
            auth_expectation: AuthExpectation::SubscriptionLogin,
            capabilities: snap(flags),
            config_identity: "cfg".into(),
            system_prompt_revision: Some("rev-7".into()),
            content: ContentPolicy::default(),
        }
    }

    fn input(resume: TurnResume) -> TurnInput {
        TurnInput {
            input_id: "in-1".into(),
            text: "hello 世界".into(),
            context_blocks: vec![],
            resume,
            system_prompt_revision: Some("rev-7".into()),
            system_prompt: Some("You are a chat assistant.\nline2".into()),
            max_vendor_turns: None,
            turn_deadline_ms: 60_000,
        }
    }

    fn broker() -> McpEndpointSpec {
        McpEndpointSpec {
            server_name: "onemind".into(),
            broker_tool_names: vec![
                "mcp__onemind__search_notes".into(),
                "mcp__onemind__read_note".into(),
            ],
            stdio_command: Some(vec![
                "/usr/libexec/onemind-mcp-bridge".into(),
                "--task".into(),
            ]),
            http_url: None,
            auth_token: Some("brk-3f9c…cret".into()),
        }
    }

    const PLAIN_FLAGS: &[&str] = &[
        "plain-chat",
        "owned-mcp-chat",
        "strict-mcp-config",
        "safe-mode",
        "permission-prompts-none",
    ];

    #[test]
    fn plain_profile_argv_is_pinned() {
        let plan = build_launch_plan(
            &target(NativeProfile::PlainChat, PLAIN_FLAGS),
            &input(TurnResume::Fresh {
                seed_history: vec![],
            }),
            None,
            PathBuf::from("/run/x"),
        )
        .unwrap();
        let flags = flag_set(&plan.argv);
        assert_eq!(plan.argv[0], "/usr/local/bin/claude");
        assert!(plan.argv.contains(&"-p".to_string()));
        assert_eq!(
            flags.get("--output-format").and_then(|v| v.as_deref()),
            Some("stream-json")
        );
        assert_eq!(
            flags.get("--input-format").and_then(|v| v.as_deref()),
            Some("stream-json")
        );
        assert!(plan.argv.contains(&"--safe-mode".to_string()));
        assert_eq!(
            flags.get("--disallowedTools").and_then(|v| v.as_deref()),
            Some("mcp__*")
        );
        assert_eq!(
            flags.get("--setting-sources").and_then(|v| v.as_deref()),
            Some("")
        );
        assert_eq!(flags.get("--tools").and_then(|v| v.as_deref()), Some(""));
        assert_eq!(
            flags.get("--permission-prompts").and_then(|v| v.as_deref()),
            Some("none")
        );
        assert_eq!(
            flags.get("--max-turns").and_then(|v| v.as_deref()),
            Some("1")
        );
        assert_eq!(
            flags.get("--model").and_then(|v| v.as_deref()),
            Some("sonnet")
        );
        assert_eq!(
            flags.get("--effort").and_then(|v| v.as_deref()),
            Some("high")
        );
        assert_eq!(
            flags.get("--mcp-config").and_then(|v| v.as_deref()),
            Some("/run/x/mcp.json")
        );
        assert_eq!(
            flags.get("--system-prompt-file").and_then(|v| v.as_deref()),
            Some("/run/x/prompt.txt")
        );
        assert!(!plan.argv.iter().any(|a| a.contains("chat assistant")));
        assert_eq!(plan.permission_policy, PermissionPromptPolicy::None);
    }

    #[test]
    fn fresh_assigns_bound_session_id_exact_resumes() {
        let fresh = build_launch_plan(
            &target(NativeProfile::PlainChat, PLAIN_FLAGS),
            &input(TurnResume::Fresh {
                seed_history: vec![],
            }),
            None,
            PathBuf::from("/run/x"),
        )
        .unwrap();
        let flags = flag_set(&fresh.argv);
        let bound = fresh.native_session_id.clone().expect("fresh binds id");
        assert_eq!(
            flags.get("--session-id").and_then(|v| v.as_deref()),
            Some(bound.as_str())
        );
        assert!(!fresh.argv.iter().any(|a| a == "--resume"));
        // The real CLI accepts only a canonical hyphenated UUID.
        assert_eq!(bound.len(), 36);
        assert_eq!(bound, bound.to_lowercase());
        assert_eq!(bound.matches('-').count(), 4, "8-4-4-4-12 hyphenated shape");
        assert!(Uuid::parse_str(&bound).is_ok());
        // Stable cwd is the sibling of the per-run dir, not the dir itself.
        assert_eq!(fresh.cwd, PathBuf::from("/run/cwd"));
        assert_eq!(fresh.per_run_dir, PathBuf::from("/run/x"));

        let exact = build_launch_plan(
            &target(NativeProfile::PlainChat, PLAIN_FLAGS),
            &input(TurnResume::Exact {
                vendor_session_id: "sess-42".into(),
            }),
            None,
            PathBuf::from("/run/x"),
        )
        .unwrap();
        let flags = flag_set(&exact.argv);
        assert_eq!(
            flags.get("--resume").and_then(|v| v.as_deref()),
            Some("sess-42")
        );
        assert_eq!(exact.native_session_id.as_deref(), Some("sess-42"));
        assert!(!exact.argv.iter().any(|a| a == "--session-id"));
    }

    #[test]
    fn fork_is_refused_not_downgraded() {
        let err = build_launch_plan(
            &target(NativeProfile::PlainChat, PLAIN_FLAGS),
            &input(TurnResume::Fork {
                vendor_session_id: "s".into(),
                expected_parent: "p".into(),
            }),
            None,
            PathBuf::from("/run/x"),
        )
        .unwrap_err();
        assert_eq!(err.code, "vendor_fork_unqualified");
    }

    #[test]
    fn mcp_profile_pins_broker_only() {
        let plan = build_launch_plan(
            &target(NativeProfile::OwnedMcpChat, PLAIN_FLAGS),
            &input(TurnResume::Fresh {
                seed_history: vec![],
            }),
            Some(&broker()),
            PathBuf::from("/run/x"),
        )
        .unwrap();
        let flags = flag_set(&plan.argv);
        assert_eq!(
            flags.get("--allowedTools").and_then(|v| v.as_deref()),
            Some("mcp__onemind__search_notes,mcp__onemind__read_note")
        );
        assert_eq!(
            flags.get("--max-turns").and_then(|v| v.as_deref()),
            Some("30")
        );
        assert!(!plan.argv.iter().any(|a| a == "--safe-mode"));
        match plan.permission_policy {
            PermissionPromptPolicy::PreapprovedBroker { broker_tool_names } => {
                assert_eq!(broker_tool_names.len(), 2);
            }
            other => panic!("expected preapproved policy, got {other:?}"),
        }
        let cfg = mcp_config_json(Some(&broker())).unwrap();
        assert!(cfg.contains("\"onemind\""));
        assert!(cfg.contains("onemind-mcp-bridge"));
        assert!(!cfg.contains("token"));
    }

    #[test]
    fn profile_endpoint_crossings_are_refused() {
        // MCP spec on plain profile.
        let err = build_launch_plan(
            &target(NativeProfile::PlainChat, PLAIN_FLAGS),
            &input(TurnResume::Fresh {
                seed_history: vec![],
            }),
            Some(&broker()),
            PathBuf::from("/run/x"),
        )
        .unwrap_err();
        assert_eq!(err.code, "mcp_on_plain_profile");
        // Missing endpoint on MCP profile.
        let err = build_launch_plan(
            &target(NativeProfile::OwnedMcpChat, PLAIN_FLAGS),
            &input(TurnResume::Fresh {
                seed_history: vec![],
            }),
            None,
            PathBuf::from("/run/x"),
        )
        .unwrap_err();
        assert_eq!(err.code, "mcp_endpoint_missing");
        // Both endpoint shapes set.
        let mut bad = broker();
        bad.http_url = Some("http://127.0.0.1:9/x".into());
        let err = build_launch_plan(
            &target(NativeProfile::OwnedMcpChat, PLAIN_FLAGS),
            &input(TurnResume::Fresh {
                seed_history: vec![],
            }),
            Some(&bad),
            PathBuf::from("/run/x"),
        )
        .unwrap_err();
        assert_eq!(err.code, "mcp_endpoint_shape");
        // Empty broker names.
        let mut bad = broker();
        bad.broker_tool_names.clear();
        let err = build_launch_plan(
            &target(NativeProfile::OwnedMcpChat, PLAIN_FLAGS),
            &input(TurnResume::Fresh {
                seed_history: vec![],
            }),
            Some(&bad),
            PathBuf::from("/run/x"),
        )
        .unwrap_err();
        assert_eq!(err.code, "mcp_endpoint_shape");
    }

    #[test]
    fn unknown_or_unsupported_capability_blocks_plan() {
        let err = build_launch_plan(
            &target(
                NativeProfile::PlainChat,
                &[
                    "owned-mcp-chat",
                    "~plain-chat",
                    "strict-mcp-config",
                    "safe-mode",
                    "permission-prompts-none",
                ],
            ), // plain unknown
            &input(TurnResume::Fresh {
                seed_history: vec![],
            }),
            None,
            PathBuf::from("/run/x"),
        )
        .unwrap_err();
        assert_eq!(err.code, "capability_unknown");
        assert!(err.retryable);

        let err = build_launch_plan(
            &target(
                NativeProfile::OwnedMcpChat,
                &[
                    "plain-chat",
                    "strict-mcp-config",
                    "safe-mode",
                    "permission-prompts-none",
                ],
            ), // mcp unsupported
            &input(TurnResume::Fresh {
                seed_history: vec![],
            }),
            Some(&broker()),
            PathBuf::from("/run/x"),
        )
        .unwrap_err();
        assert_eq!(err.code, "capability_unsupported");
    }

    #[test]
    fn backend_mismatch_and_empty_input_refused() {
        let mut t = target(NativeProfile::PlainChat, PLAIN_FLAGS);
        t.backend = "other".into();
        let err = build_launch_plan(
            &t,
            &input(TurnResume::Fresh {
                seed_history: vec![],
            }),
            None,
            PathBuf::from("/run/x"),
        )
        .unwrap_err();
        assert_eq!(err.code, "backend_mismatch");

        let mut i = input(TurnResume::Fresh {
            seed_history: vec![],
        });
        i.text = "   ".into();
        let err = build_launch_plan(
            &target(NativeProfile::PlainChat, PLAIN_FLAGS),
            &i,
            None,
            PathBuf::from("/run/x"),
        )
        .unwrap_err();
        assert_eq!(err.code, "empty_input");
    }

    #[test]
    fn fingerprint_is_stable_and_path_free_of_secrets() {
        let mk = || {
            build_launch_plan(
                &target(NativeProfile::PlainChat, PLAIN_FLAGS),
                &input(TurnResume::Exact {
                    vendor_session_id: "sess-42".into(),
                }),
                None,
                PathBuf::from("/run/x"),
            )
            .unwrap()
        };
        assert_eq!(mk().fingerprint, mk().fingerprint);
        let plan = mk();
        // Config identity changes with the prompt revision.
        let mut t2 = target(NativeProfile::PlainChat, PLAIN_FLAGS);
        t2.system_prompt_revision = Some("rev-8".into());
        let other = build_launch_plan(
            &t2,
            &input(TurnResume::Exact {
                vendor_session_id: "sess-42".into(),
            }),
            None,
            PathBuf::from("/run/x"),
        )
        .unwrap();
        assert_ne!(plan.fingerprint, other.fingerprint);
    }

    #[test]
    fn per_run_files_are_private_and_complete() {
        let dir = tempfile::tempdir().unwrap();
        write_per_run_files(dir.path(), Some("prompt body éé"), Some(&broker())).unwrap();
        use std::os::unix::fs::PermissionsExt;
        for name in ["mcp.json", "settings.json", "prompt.txt"] {
            let md = std::fs::metadata(dir.path().join(name)).unwrap();
            assert_eq!(md.permissions().mode() & 0o777, 0o600, "{name}");
        }
        let prompt = std::fs::read_to_string(dir.path().join("prompt.txt")).unwrap();
        assert_eq!(prompt, "prompt body éé");
        // Plain profile: no prompt file, and a stale one is removed.
        write_per_run_files(dir.path(), None, None).unwrap();
        assert!(!dir.path().join("prompt.txt").exists());
        let cfg: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join("mcp.json")).unwrap())
                .unwrap();
        assert_eq!(cfg["mcpServers"].as_object().unwrap().len(), 0);
    }

    #[test]
    fn http_endpoint_carries_bearer_header_only_in_private_config() {
        let spec = McpEndpointSpec {
            server_name: "onemind".into(),
            broker_tool_names: vec!["mcp__onemind__search".into()],
            stdio_command: None,
            http_url: Some("http://127.0.0.1:41121/mcp".into()),
            auth_token: Some("brk-secret-1".into()),
        };
        let cfg = mcp_config_json(Some(&spec)).unwrap();
        assert!(cfg.contains("\"Authorization\": \"Bearer brk-secret-1\""));
        // The token never enters argv: the plan only references the file.
        let plan = build_launch_plan(
            &target(NativeProfile::OwnedMcpChat, PLAIN_FLAGS),
            &input(TurnResume::Fresh {
                seed_history: vec![],
            }),
            Some(&spec),
            PathBuf::from("/run/x"),
        )
        .unwrap();
        assert!(!plan.argv.iter().any(|a| a.contains("brk-secret-1")));
        // Absent token: no headers key at all.
        let no_token = McpEndpointSpec {
            auth_token: None,
            ..spec
        };
        let cfg = mcp_config_json(Some(&no_token)).unwrap();
        assert!(!cfg.contains("headers"));
    }

    #[test]
    fn env_additions_are_noncredential_and_scrub_target_recorded() {
        let plan = build_launch_plan(
            &target(NativeProfile::PlainChat, PLAIN_FLAGS),
            &input(TurnResume::Fresh {
                seed_history: vec![],
            }),
            None,
            PathBuf::from("/run/x"),
        )
        .unwrap();
        assert!(plan.env.iter().all(|(k, _)| !k.contains("TOKEN")));
        assert!(
            plan.env
                .iter()
                .any(|(k, _)| k == "CLAUDE_CODE_DISABLE_BACKGROUND_TASKS")
        );
        // The ambient interrupted-turn resume variable is never *set* by us;
        // the transport scrubs it from the inherited environment.
        assert!(
            plan.env
                .iter()
                .all(|(k, _)| k != "CLAUDE_CODE_RESUME_INTERRUPTED_TURN")
        );
    }
}
