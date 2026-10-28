//! `run_command` and `run_tests`: bounded shell execution under a workspace
//! lease. Port of `packages/toolpacks-coding/src/exec-tools.ts`.
//!
//! Every command takes the exclusive workspace lease before it spawns and
//! holds it until the process tree is gone, so no file mutation can race a
//! running command. The macOS sandbox is opt-out (`sandbox: "off"`): with
//! `required` (the default) a host without `sandbox-exec` is refused rather
//! than silently running unsandboxed.

use std::collections::{HashMap, HashSet};
use std::process::id as process_id;
use std::sync::Arc;

use agentkit_core::{Tool, ToolContext};
use regex::Regex;
use serde_json::{Map, Value, json};

use crate::args::{as_record, opt_int, opt_string, req_string};
use crate::context::{Outcome, PackEnv, ToolEffect, ToolSpec, define_tool_with_effect_context};
use crate::coordination::{AcquireWorkspaceInput, LeaseHolder};
use crate::errors::CodingError;
use crate::paths::{ConfineOptions, PathAccess, confine};
use crate::process::{RunSpec, spawn_owned_execution, startup_recovery};
use crate::sandbox::{
    SandboxEnvironment, SandboxPolicy, sandbox_available, sandbox_profile, toolchain_paths,
};
use crate::types::SandboxMode;

// `AUTH(?!OR)`: GIT_AUTHOR_NAME/EMAIL are commit identity, not credentials.
static SECRET_NAME: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r"(?i)(TOKEN|SECRET|PASSWORD|PASSWD|KEY|CREDENTIAL|COOKIE|SESSION|PRIVATE)")
        .expect("secret name pattern")
});
static AGENT_VARS: std::sync::LazyLock<HashSet<&'static str>> =
    std::sync::LazyLock::new(|| HashSet::from(["SSH_AUTH_SOCK", "GPG_AGENT_INFO"]));
/// A URL with userinfo (`scheme://user:pass@host`) carries a credential.
static CREDENTIAL_URL: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r"(?i)^[a-z][a-z0-9+.-]*://[^/\s:@]+:[^/\s@]+@").expect("credential url pattern")
});
const MAX_COMMAND_CHARS: usize = 16_384;

/// JS `SECRET_NAME.test(name)` including the `AUTH(?!OR)` lookahead the regex
/// crate cannot express: match when some occurrence of `auth` (case-insensitive)
/// is NOT followed by `or`.
fn auth_unless_author(name: &str) -> bool {
    let bytes = name.to_lowercase();
    let bytes = bytes.as_bytes();
    let mut index = 0;
    while index + 4 <= bytes.len() {
        if &bytes[index..index + 4] == b"auth" {
            let followed_by_or = index + 6 <= bytes.len() && &bytes[index + 4..index + 6] == b"or";
            if !followed_by_or {
                return true;
            }
        }
        index += 1;
    }
    false
}

/// Process env minus secrets; PATH, HOME, LANG and TERM always survive.
pub fn sanitized_env(source: &HashMap<String, String>, scratch: &str) -> HashMap<String, String> {
    let mut env = HashMap::new();
    for (name, value) in source {
        if AGENT_VARS.contains(name.as_str())
            || SECRET_NAME.is_match(name)
            || auth_unless_author(name)
            || CREDENTIAL_URL.is_match(value)
        {
            continue;
        }
        env.insert(name.clone(), value.clone());
    }
    env.insert(
        "PATH".to_owned(),
        source
            .get("PATH")
            .cloned()
            .unwrap_or_else(|| "/usr/bin:/bin:/usr/sbin:/sbin".to_owned()),
    );
    env.insert(
        "HOME".to_owned(),
        source
            .get("HOME")
            .cloned()
            .unwrap_or_else(|| scratch.to_owned()),
    );
    env.insert(
        "LANG".to_owned(),
        source
            .get("LANG")
            .cloned()
            .unwrap_or_else(|| "C.UTF-8".to_owned()),
    );
    env.insert(
        "TERM".to_owned(),
        source
            .get("TERM")
            .cloned()
            .unwrap_or_else(|| "dumb".to_owned()),
    );
    env.insert("TMPDIR".to_owned(), scratch.to_owned());
    env
}

fn process_env() -> HashMap<String, String> {
    // Node exposes every inherited entry with a lossy UTF-8 decode; Rust's
    // `vars()` silently DROPS non-UTF-8 pairs, so the child would lack them.
    std::env::vars_os()
        .map(|(key, value)| {
            (
                key.to_string_lossy().into_owned(),
                value.to_string_lossy().into_owned(),
            )
        })
        .collect()
}

/// JS `String.prototype.slice(0, n)` counts UTF-16 code units.
fn slice_utf16(value: &str, max_units: usize) -> String {
    let units: Vec<u16> = value.encode_utf16().take(max_units).collect();
    String::from_utf16_lossy(&units)
}

struct ShellRequest {
    tool: &'static str,
    command: String,
    cwd: Option<String>,
    timeout_sec: u64,
}

async fn run_shell(
    env: Arc<PackEnv>,
    ctx: ToolContext,
    request: ShellRequest,
) -> Result<Outcome, CodingError> {
    let sandboxed = env.sandbox == SandboxMode::Required;
    if sandboxed
        && !sandbox_available(&SandboxEnvironment {
            platform: env.platform.clone(),
            sandbox_exec_path: env.sandbox_exec_path.clone(),
        })
    {
        return Err(CodingError::new(
            "shell_isolation_unavailable",
            format!(
                "sandbox-exec is not available on this host ({}); refusing to run an unsandboxed command. Set sandbox: \"off\" explicitly to accept that.",
                env.platform
            ),
        ));
    }
    let root = env.root().await?;
    let cwd = confine(
        &root,
        request.cwd.as_deref().unwrap_or("."),
        ConfineOptions {
            access: PathAccess::Read,
            allow_sensitive: env.allow_sensitive,
        },
    )
    .await?;
    let info = tokio::fs::metadata(&cwd.path).await.ok();
    if !info.is_some_and(|info| info.is_dir()) {
        return Err(CodingError::new(
            "not_a_directory",
            "cwd is not a directory",
        ));
    }
    let scratch = env.scratch().await?;
    let base = process_env();
    let (file, args) = if sandboxed {
        let profile = sandbox_profile(&SandboxPolicy {
            root: root.clone(),
            scratch: scratch.clone(),
            network: env.network,
            allow_git_writes: env.allow_git_writes,
            readable: toolchain_paths(&base),
        })?;
        (
            env.sandbox_exec_path.clone(),
            vec![
                "-p".to_owned(),
                profile,
                "/bin/bash".to_owned(),
                "-c".to_owned(),
                request.command.clone(),
            ],
        )
    } else {
        (
            "/bin/bash".to_owned(),
            vec!["-c".to_owned(), request.command.clone()],
        )
    };
    // Every command may write, so every command takes the exclusive lease; it
    // is held from before spawn until the process tree has been torn down
    // (and, if survivors remain, until they are gone or a short re-check
    // gives up). The execution identity is minted BEFORE the lease: the same
    // id names the `AGENTKIT_EXEC_ID` marker inherited by the whole process
    // tree and the durable ownership record, so cleanup evidence names the
    // exact execution, not a pid that can be recycled (ADR 0020).
    let execution_id = format!("{}:{}", process_id(), uuid::Uuid::new_v4());
    // A crashed agent never ran its teardown: no modifying effect starts
    // before the once-per-process startup sweep has completed (ADR 0020).
    startup_recovery().await;
    let holder = LeaseHolder {
        chat_id: ctx.chat_id.clone(),
        run_id: ctx.run_id.clone(),
        // The host supplies the model call's stable identity; the
        // durable intent binds run + tool call + attempt (ADR 0020).
        tool_call_id: ctx.tool_call_id.clone(),
        tool: request.tool.to_owned(),
        label: slice_utf16(&request.command, 200),
    };
    // B5: durable PRE-SPAWN / PRE-ADMISSION intent, written before the
    // lease is admitted and long before the child spawns. A crash at any
    // point of admit → spawn → effect → completion therefore leaves an
    // identifiable ownership record — stable execution nonce, owner pid,
    // and (once the driver confirms the spawn) the process group and
    // spawn time — so the next boot can reconcile the crashed run's
    // process group instead of guessing from a bare pid. No durable
    // intent, no spawn: the failure is returned and the workspace stays
    // unmodified.
    env.coordinator
        .record_execution_intent(&crate::coordination::ExecutionIntent {
            root: root.clone(),
            identity: crate::coordination::CommandCleanupIdentity {
                execution_id: execution_id.clone(),
                owner_pid: i64::from(process_id()),
                pgid: None,
                spawned_at_ms: 0,
            },
            holder: holder.clone(),
            spawned: false,
        })
        .await?;
    let lease = env
        .coordinator
        .acquire_workspace(AcquireWorkspaceInput {
            root: root.clone(),
            holder: holder.clone(),
            cancellation: Some(ctx.cancellation.clone()),
            max_wait_ms: Some(env.coordination_wait_ms),
            execution_id: Some(execution_id.clone()),
        })
        .await?;
    let log_name = format!(
        "command-{}-{}.log",
        (env.clock)(),
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    );
    // The lease and every cleanup step (teardown, survivor recheck,
    // quarantine, release) are owned by the detached driver task, never by
    // this awaitable future: the core harness drops a tool future outright
    // when its budget `select!` completes, and a lease dropped here would
    // release the gate through `WorkspaceLease::drop` while the command was
    // still running, skipping quarantine (B1). Awaiting the driver is the
    // normal path; being dropped here only stops us from listening.
    let execution = spawn_owned_execution(
        RunSpec {
            file,
            args,
            cwd: cwd.path.clone(),
            env: sanitized_env(&base, &scratch),
            timeout_ms: request.timeout_sec * 1000,
            cancellation: Some(ctx.cancellation.clone()),
            scratch: scratch.clone(),
            log_name,
            limits: env.limits.clone(),
            execution_id,
            pgid_slot: None,
        },
        lease,
        Arc::clone(&env.coordinator),
        root.clone(),
        holder,
    );
    let outcome = execution.await_result().await;
    let run = outcome.run;
    let stragglers = outcome.stragglers;
    if let Some(error) = outcome.quarantine_error {
        return Err(error);
    }
    if let Some(spawn_error) = &run.spawn_error {
        return Err(CodingError::new(
            "spawn_failed",
            slice_utf16(spawn_error, 500),
        ));
    }
    let footer = if run.timed_out {
        format!(
            "[timed out after {}s; process group killed{}]",
            request.timeout_sec,
            run.signal
                .as_ref()
                .map(|signal| format!(" ({signal})"))
                .unwrap_or_default()
        )
    } else if run.aborted {
        "[aborted; process group killed]".to_owned()
    } else if let Some(signal) = &run.signal {
        format!("[killed by {signal}]")
    } else {
        format!(
            "[exit code {}]",
            run.exit_code
                .map(|code| code.to_string())
                .unwrap_or_else(|| "null".to_owned())
        )
    };
    let parts = [
        run.output.trim_end_matches('\n').to_owned(),
        run.notice.clone(),
        footer,
    ]
    .into_iter()
    .filter(|part| !part.is_empty())
    .collect::<Vec<_>>();
    let ok = run.exit_code == Some(0) && !run.timed_out && !run.aborted;
    let mut details = Map::new();
    details.insert("exitCode".to_owned(), json!(run.exit_code));
    details.insert("signal".to_owned(), json!(run.signal));
    details.insert("timedOut".to_owned(), json!(run.timed_out));
    details.insert(
        "teardown".to_owned(),
        serde_json::to_value(&run.teardown).unwrap_or(Value::Null),
    );
    if let Some(log_path) = &run.log_path {
        details.insert("fullLog".to_owned(), json!(log_path));
    }
    Ok(Outcome {
        ok,
        text: parts.join("\n"),
        summary: Some(if run.timed_out {
            format!("timed out after {}s", request.timeout_sec)
        } else if run.aborted {
            "aborted".to_owned()
        } else if let Some(signal) = &run.signal {
            format!("killed by {signal}")
        } else {
            format!(
                "exit code {}",
                run.exit_code
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "null".to_owned())
            )
        }),
        truncated: run.truncated,
        warnings: [
            if sandboxed {
                vec![]
            } else {
                vec!["sandbox is off: the command ran unconfined".to_owned()]
            },
            if run.teardown.survivors.is_empty() {
                vec![]
            } else {
                vec![format!(
                    "process_survivors: {}",
                    run.teardown
                        .survivors
                        .iter()
                        .map(|pid| pid.to_string())
                        .collect::<Vec<_>>()
                        .join(",")
                )]
            },
            if stragglers.is_empty() && run.teardown.scan != crate::process::TeardownScan::Unsupported {
                vec![]
            } else {
                let who = if stragglers.is_empty() {
                    "unknown survivors (teardown scan unsupported on this platform)".to_owned()
                } else {
                    stragglers
                        .iter()
                        .map(|pid| pid.to_string())
                        .collect::<Vec<_>>()
                        .join(",")
                };
                vec![format!(
                    "lease released while cleanup was not positively established: {who}"
                )]
            },
            if stragglers.is_empty() && run.teardown.scan != crate::process::TeardownScan::Unsupported {
                vec![]
            } else {
                let who = if stragglers.is_empty() {
                    "unknown survivors (teardown scan unsupported on this platform)".to_owned()
                } else {
                    stragglers
                        .iter()
                        .map(|pid| pid.to_string())
                        .collect::<Vec<_>>()
                        .join(",")
                };
                vec![format!(
                    "workspace quarantined until these processes exit or recovery is recorded: {who}"
                )]
            },
        ]
        .concat(),
        details: Some(Value::Object(details)),
    })
}

fn shell_effect_context(
    env: &PackEnv,
    sandboxed: bool,
    network: bool,
    test_commands: Option<&[String]>,
) -> Value {
    let mut context = Map::new();
    context.insert("workspace".into(), env.workspace_root.clone().into());
    context.insert("platform".into(), env.platform.clone().into());
    context.insert(
        "sandbox".into(),
        if sandboxed { "required" } else { "off" }.into(),
    );
    context.insert("network".into(), network.into());
    if sandboxed {
        context.insert(
            "sandboxExecPath".into(),
            env.sandbox_exec_path.clone().into(),
        );
    }
    context.insert("runner".into(), "bash".into());
    if let Some(commands) = test_commands {
        context.insert("testCommands".into(), json!(commands));
    }
    Value::Object(context)
}

pub fn run_command_tool(env: &Arc<PackEnv>) -> Arc<dyn Tool> {
    let l = env.limits.clone();
    let sandboxed = env.sandbox == SandboxMode::Required;
    let network = env.network;
    let description = format!(
        "Run a shell command (bash -c) in the workspace{}. Use it for builds, tests and tooling. Do NOT use it to read or search files, list directories or inspect git: use read_file, list_dir, find_files, code_search, repo_map and git_inspect, which are cheaper and safer. Output keeps the last {} lines / {} KB. Default timeout {}s, max {}s.",
        if sandboxed {
            format!(
                ", inside a macOS sandbox: writes only in the workspace, no network{}{}, no access to credentials",
                if network { " restrictions lifted" } else { "" },
                "",
            )
        } else {
            " WITHOUT a sandbox".to_owned()
        },
        l.command_tail_max_lines,
        // JS interpolates `bytes / 1024` as an IEEE-754 number rendered by
        // Number-to-string; match it, not integer division.
        l.command_tail_max_bytes as f64 / 1024.0,
        l.command_default_timeout_sec,
        l.command_max_timeout_sec,
    );
    let input_schema = json!({
        "type": "object",
        "properties": {
            "command": { "type": "string", "description": "Shell command line." },
            "cwd": {
                "type": "string",
                "description": "Directory inside the workspace. Default: the root."
            },
            "timeoutSec": { "type": "integer", "minimum": 1, "maximum": l.command_max_timeout_sec }
        },
        "required": ["command"],
        "additionalProperties": false
    });
    let env = Arc::clone(env);
    let effect = shell_effect_context(&env, sandboxed, network, None);
    define_tool_with_effect_context(
        ToolSpec {
            name: "run_command",
            effect: ToolEffect::Write,
            capability: "process.exec",
            description,
            input_schema,
            timeout_ms: (l.command_max_timeout_sec + 30) * 1000,
        },
        effect,
        move |ctx: ToolContext, raw: Value| {
            let env = Arc::clone(&env);
            let l = l.clone();
            async move {
                let input = as_record(&raw)?;
                run_shell(
                    env,
                    ctx,
                    ShellRequest {
                        tool: "run_command",
                        command: req_string(input, "command", MAX_COMMAND_CHARS)?,
                        cwd: opt_string(input, "cwd", 4096)?,
                        timeout_sec: opt_int(
                            input,
                            "timeoutSec",
                            1,
                            l.command_max_timeout_sec as i64,
                        )?
                        .unwrap_or(l.command_default_timeout_sec as i64)
                            as u64,
                    },
                )
                .await
            }
        },
    )
}

pub fn run_tests_tool(env: &Arc<PackEnv>) -> Arc<dyn Tool> {
    let l = env.limits.clone();
    let test_commands = env.test_commands.clone();
    let allowed = if !test_commands.is_empty() {
        test_commands
            .iter()
            .map(|command| format!("\"{command}\""))
            .collect::<Vec<_>>()
            .join(", ")
    } else {
        "(none configured)".to_owned()
    };
    let description = format!(
        "Run one of the project's configured test commands, exactly as written: {allowed}. Same sandbox and output limits as run_command."
    );
    let input_schema = json!({
        "type": "object",
        "properties": {
            "command": {
                "type": "string",
                "description": "One of the allowed test commands, verbatim."
            }
        },
        "required": ["command"],
        "additionalProperties": false
    });
    let env = Arc::clone(env);
    let effect = shell_effect_context(
        &env,
        env.sandbox == SandboxMode::Required,
        env.network,
        Some(&test_commands),
    );
    define_tool_with_effect_context(
        ToolSpec {
            name: "run_tests",
            effect: ToolEffect::Write,
            capability: "process.exec",
            description,
            input_schema,
            timeout_ms: (l.command_max_timeout_sec + 30) * 1000,
        },
        effect,
        move |ctx: ToolContext, raw: Value| {
            let env = Arc::clone(&env);
            let l = l.clone();
            let test_commands = test_commands.clone();
            let allowed = allowed.clone();
            async move {
                let command = req_string(as_record(&raw)?, "command", MAX_COMMAND_CHARS)?;
                if !test_commands.contains(&command) {
                    return Err(CodingError::new(
                        "test_command_not_allowed",
                        format!(
                            "\"{}\" is not a configured test command. Allowed: {allowed}",
                            slice_utf16(&command, 200)
                        ),
                    ));
                }
                run_shell(
                    env,
                    ctx,
                    ShellRequest {
                        tool: "run_tests",
                        command,
                        cwd: None,
                        timeout_sec: l.command_max_timeout_sec,
                    },
                )
                .await
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::default_in_process_coordinator;
    use crate::limits::CodingLimits;
    use crate::types::system_clock;
    use agentkit_core::{ToolLimits, ToolResult};
    use std::path::PathBuf;
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    struct TempRoot {
        root: PathBuf,
    }

    impl TempRoot {
        async fn new(tag: &str) -> Self {
            let root =
                std::env::temp_dir().join(format!("ak-exec-tools-{tag}-{}", uuid::Uuid::new_v4()));
            tokio::fs::create_dir_all(&root).await.unwrap();
            let root = tokio::fs::canonicalize(&root).await.unwrap();
            Self { root }
        }

        async fn put_dir(&self, rel: &str) {
            tokio::fs::create_dir_all(self.root.join(rel))
                .await
                .unwrap();
        }

        async fn put(&self, rel: &str, content: &str) {
            let path = self.root.join(rel);
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await.unwrap();
            }
            tokio::fs::write(&path, content).await.unwrap();
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn env_for(
        root: &std::path::Path,
        sandbox: SandboxMode,
        test_commands: Vec<String>,
    ) -> Arc<PackEnv> {
        Arc::new(PackEnv::new(crate::context::EnvInit {
            workspace_root: root.to_string_lossy().into_owned(),
            limits: CodingLimits::default(),
            allow_sensitive: false,
            clock: system_clock(),
            sandbox,
            network: false,
            allow_git_writes: false,
            test_commands,
            platform: "linux".to_owned(),
            sandbox_exec_path: String::new(),
            coordinator: default_in_process_coordinator(),
            coordination_wait_ms: 20_000,
        }))
    }

    fn context() -> ToolContext {
        ToolContext {
            suspension: None,
            run_id: "test-run".to_owned(),
            tool_call_id: None,
            attempt_id: None,
            chat_id: None,
            scope_id: None,
            user_id: None,
            bindings: vec![],
            limits: ToolLimits::default(),
            cancellation: agentkit_core::CancellationToken::new(),
            dispatch_guard: None,
            metadata: serde_json::Map::new(),
        }
    }

    fn text(result: &ToolResult) -> String {
        match &result.model_data {
            Some(Value::String(text)) => text.clone(),
            Some(other) => other.to_string(),
            None => String::new(),
        }
    }

    async fn run(tool: &Arc<dyn Tool>, input: Value) -> ToolResult {
        tool.execute(context(), input).await.unwrap()
    }

    async fn run_with(tool: &Arc<dyn Tool>, input: Value, token: CancellationToken) -> ToolResult {
        let mut ctx = context();
        ctx.cancellation = token;
        tool.execute(ctx, input).await.unwrap()
    }

    /// Pids whose /proc cmdline contains `needle` (Linux stand-in for the TS
    /// `alive()` pgrep probe).
    fn alive(needle: &str) -> Vec<i32> {
        let mut found = vec![];
        if let Ok(entries) = std::fs::read_dir("/proc") {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if !name.chars().all(|c| c.is_ascii_digit()) {
                    continue;
                }
                let Ok(cmdline) = std::fs::read(entry.path().join("cmdline")) else {
                    continue;
                };
                if String::from_utf8_lossy(&cmdline).contains(needle) {
                    found.push(name.parse().unwrap());
                }
            }
        }
        found
    }

    fn map(entries: &[(&str, &str)]) -> HashMap<String, String> {
        entries
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn sanitized_env_fills_defaults_and_points_tmpdir_at_scratch() {
        let env = sanitized_env(&map(&[("FOO", "1"), ("GITHUB_TOKEN", "x")]), "/scratch");
        assert_eq!(env.get("FOO").map(String::as_str), Some("1"));
        assert!(!env.contains_key("GITHUB_TOKEN"));
        assert_eq!(
            env.get("PATH").map(String::as_str),
            Some("/usr/bin:/bin:/usr/sbin:/sbin")
        );
        assert_eq!(env.get("TERM").map(String::as_str), Some("dumb"));
        assert_eq!(env.get("TMPDIR").map(String::as_str), Some("/scratch"));
    }

    #[test]
    fn sanitized_env_keeps_git_commit_identity() {
        let env = sanitized_env(
            &map(&[
                ("GIT_AUTHOR_NAME", "a"),
                ("GIT_AUTHOR_EMAIL", "a@x"),
                ("OAUTH_CLIENT", "s"),
            ]),
            "/scratch",
        );
        assert_eq!(env.get("GIT_AUTHOR_NAME").map(String::as_str), Some("a"));
        assert_eq!(env.get("GIT_AUTHOR_EMAIL").map(String::as_str), Some("a@x"));
        assert!(!env.contains_key("OAUTH_CLIENT"));
    }

    #[test]
    fn sanitized_env_drops_agent_sockets_auth_names_and_credential_urls() {
        let env = sanitized_env(
            &map(&[
                ("SSH_AUTH_SOCK", "/tmp/agent.sock"),
                ("GPG_AGENT_INFO", "x"),
                ("MY_COOKIE", "c"),
                ("SESSION_ID", "s"),
                ("DB_PASSWD", "p"),
                ("MY_PRIVATE_THING", "k"),
                ("REMOTE", "https://user:pw@example.com/repo.git"),
                ("PLAIN_URL", "https://example.com/repo.git"),
                ("ALSO_OK", "ssh://git@example.com/x"),
                ("LANG", "en_US.UTF-8"),
                ("FOO", "1"),
            ]),
            "/scratch",
        );
        for key in [
            "SSH_AUTH_SOCK",
            "GPG_AGENT_INFO",
            "MY_COOKIE",
            "SESSION_ID",
            "DB_PASSWD",
            "MY_PRIVATE_THING",
            "REMOTE",
        ] {
            assert!(!env.contains_key(key), "{key} must be dropped");
        }
        assert_eq!(
            env.get("PLAIN_URL").map(String::as_str),
            Some("https://example.com/repo.git")
        );
        assert_eq!(
            env.get("ALSO_OK").map(String::as_str),
            Some("ssh://git@example.com/x")
        );
        assert_eq!(env.get("LANG").map(String::as_str), Some("en_US.UTF-8"));
        assert_eq!(env.get("FOO").map(String::as_str), Some("1"));
    }

    #[test]
    fn kill_process_tree_on_a_dead_pid_does_not_panic() {
        crate::process::kill_process_tree(2_000_000_000);
    }

    #[tokio::test]
    async fn runs_reports_the_exit_code_and_warns_that_the_sandbox_is_off() {
        let h = TempRoot::new("ok").await;
        let tool = run_command_tool(&env_for(&h.root, SandboxMode::Off, vec![]));
        let ok = run(&tool, json!({ "command": "echo hello; echo oops >&2" })).await;
        assert!(ok.ok);
        assert_eq!(text(&ok), "hello\noops\n[exit code 0]");
        assert_eq!(
            ok.warnings,
            vec!["sandbox is off: the command ran unconfined".to_owned()]
        );
        let details = ok.data.clone().unwrap();
        assert_eq!(details["exitCode"], json!(0));
        assert_eq!(details["signal"], Value::Null);
        assert_eq!(details["timedOut"], json!(false));
        assert_eq!(details["teardown"]["escalated"], json!(false));
        assert_eq!(details["teardown"]["survivors"], json!([]));

        let bad = run(&tool, json!({ "command": "/bin/false" })).await;
        assert!(!bad.ok);
        assert_eq!(text(&bad), "[exit code 1]");
        assert_eq!(bad.summary.as_deref(), Some("exit code 1"));
        let bad = run(&tool, json!({ "command": "exit 3" })).await;
        assert!(!bad.ok);
        assert_eq!(text(&bad), "[exit code 3]");
        assert_eq!(bad.summary.as_deref(), Some("exit code 3"));
    }

    #[tokio::test]
    async fn cwd_is_confined_and_must_be_a_directory() {
        let h = TempRoot::new("cwd").await;
        h.put_dir("sub").await;
        let tool = run_command_tool(&env_for(&h.root, SandboxMode::Off, vec![]));
        let ok = run(
            &tool,
            json!({ "command": "basename \"$PWD\"", "cwd": "sub" }),
        )
        .await;
        assert!(text(&ok).contains("sub"));
        let out = run(&tool, json!({ "command": "pwd", "cwd": ".." })).await;
        assert!(text(&out).contains("path_outside_root"));
        h.put("f.txt", "").await;
        let out = run(&tool, json!({ "command": "pwd", "cwd": "f.txt" })).await;
        assert!(text(&out).contains("not_a_directory"));
    }

    #[tokio::test]
    async fn bad_arguments_fail_without_throwing() {
        let h = TempRoot::new("badargs").await;
        let tool = run_command_tool(&env_for(&h.root, SandboxMode::Off, vec![]));
        assert!(!run(&tool, json!({ "command": "" })).await.ok);
        assert!(
            !run(&tool, json!({ "command": "x", "timeoutSec": 0 }))
                .await
                .ok
        );
        assert!(
            !run(&tool, json!({ "command": "x", "timeoutSec": 601 }))
                .await
                .ok
        );
    }

    #[tokio::test]
    #[ignore = "slow: exercises the 1s timeout and group teardown"]
    async fn timeout_kills_the_whole_process_group() {
        let h = TempRoot::new("timeout").await;
        let tool = run_command_tool(&env_for(&h.root, SandboxMode::Off, vec![]));
        let r = run(
            &tool,
            json!({ "command": "sleep 7001 & sleep 7002 & wait", "timeoutSec": 1 }),
        )
        .await;
        assert!(!r.ok);
        assert!(text(&r).contains("[timed out after 1s; process group killed"));
        assert_eq!(r.summary.as_deref(), Some("timed out after 1s"));
        assert!(alive("sleep 7001").is_empty());
        assert!(alive("sleep 7002").is_empty());
    }

    #[tokio::test]
    #[ignore = "slow: exercises cancellation teardown"]
    async fn abort_kills_the_group_and_reports_it() {
        let h = TempRoot::new("abort").await;
        let tool = run_command_tool(&env_for(&h.root, SandboxMode::Off, vec![]));
        let token = CancellationToken::new();
        let watcher = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            watcher.cancel();
        });
        let r = run_with(
            &tool,
            json!({ "command": "sleep 7003 & sleep 7004 & wait" }),
            token,
        )
        .await;
        assert!(!r.ok);
        assert_eq!(r.summary.as_deref(), Some("aborted"));
        assert!(alive("sleep 7003").is_empty());
        assert!(alive("sleep 7004").is_empty());
    }

    #[tokio::test]
    async fn required_on_a_non_darwin_host_is_refused() {
        let h = TempRoot::new("refuse").await;
        let tool = run_command_tool(&env_for(&h.root, SandboxMode::Required, vec![]));
        let r = run(&tool, json!({ "command": "echo hi" })).await;
        assert!(!r.ok);
        assert_eq!(
            text(&r),
            "shell_isolation_unavailable: sandbox-exec is not available on this host (linux); refusing to run an unsandboxed command. Set sandbox: \"off\" explicitly to accept that."
        );
        let t = run_tests_tool(&env_for(
            &h.root,
            SandboxMode::Required,
            vec!["echo hi".to_owned()],
        ));
        let r = run(&t, json!({ "command": "echo hi" })).await;
        assert!(text(&r).contains("shell_isolation_unavailable"));
    }

    #[tokio::test]
    async fn required_with_a_missing_sandbox_exec_path_is_refused() {
        let h = TempRoot::new("refuse-darwin").await;
        let env = Arc::new(PackEnv::new(crate::context::EnvInit {
            workspace_root: h.root.to_string_lossy().into_owned(),
            limits: CodingLimits::default(),
            allow_sensitive: false,
            clock: system_clock(),
            sandbox: SandboxMode::Required,
            network: false,
            allow_git_writes: false,
            test_commands: vec![],
            platform: "darwin".to_owned(),
            sandbox_exec_path: "/nonexistent/sandbox-exec".to_owned(),
            coordinator: default_in_process_coordinator(),
            coordination_wait_ms: 20_000,
        }));
        let r = run(&run_command_tool(&env), json!({ "command": "echo hi" })).await;
        assert!(!r.ok);
        assert!(text(&r).contains("shell_isolation_unavailable"));
    }

    #[tokio::test]
    async fn descriptions_follow_each_packs_own_configuration() {
        // Two packs in one process: TS builds the description inside each
        // `runCommandTool(env)` closure, so an `Off` pack must NEVER
        // advertise the other pack's sandbox posture or test commands.
        let a = TempRoot::new("desc-a").await;
        let b = TempRoot::new("desc-b").await;
        let required = run_command_tool(&env_for(&a.root, SandboxMode::Required, vec![]));
        let off = run_command_tool(&env_for(&b.root, SandboxMode::Off, vec![]));
        let required_text = required.definition().description.clone();
        let off_text = off.definition().description.clone();
        assert!(
            required_text.contains("inside a macOS sandbox"),
            "{required_text}"
        );
        assert!(
            !required_text.contains("WITHOUT a sandbox"),
            "{required_text}"
        );
        assert!(off_text.contains("WITHOUT a sandbox"), "{off_text}");
        assert!(!off_text.contains("no network"), "{off_text}");

        let tests_a = run_tests_tool(&env_for(
            &a.root,
            SandboxMode::Off,
            vec!["bun test".to_owned()],
        ));
        let tests_b = run_tests_tool(&env_for(
            &b.root,
            SandboxMode::Off,
            vec!["cargo test".to_owned()],
        ));
        assert!(tests_a.definition().description.contains("\"bun test\""));
        assert!(!tests_a.definition().description.contains("cargo test"));
        assert!(tests_b.definition().description.contains("\"cargo test\""));
        assert!(!tests_b.definition().description.contains("bun test"));
    }

    #[tokio::test]
    async fn command_description_renders_kb_like_js_numbers() {
        // TS interpolates `commandTailMaxBytes / 1024` as an IEEE-754 number
        // rendered by Number-to-string: 100_000 must advertise
        // "97.65625 KB", not integer-division "97 KB". Defaults (51200)
        // still render "50 KB".
        let h = TempRoot::new("desc-kb").await;
        let env = Arc::new(PackEnv::new(crate::context::EnvInit {
            workspace_root: h.root.to_string_lossy().into_owned(),
            limits: CodingLimits {
                command_tail_max_bytes: 100_000,
                ..CodingLimits::default()
            },
            allow_sensitive: false,
            clock: system_clock(),
            sandbox: SandboxMode::Off,
            network: false,
            allow_git_writes: false,
            test_commands: vec![],
            platform: "linux".to_owned(),
            sandbox_exec_path: String::new(),
            coordinator: default_in_process_coordinator(),
            coordination_wait_ms: 20_000,
        }));
        let text = run_command_tool(&env).definition().description.clone();
        assert!(text.contains("97.65625 KB"), "{text}");
        let default_env = env_for(&h.root, SandboxMode::Off, vec![]);
        assert!(
            run_command_tool(&default_env)
                .definition()
                .description
                .contains("50 KB")
        );
    }

    #[tokio::test]
    async fn keeps_the_tail_with_a_notice_and_a_full_log_in_scratch() {
        let h = TempRoot::new("tail").await;
        let tool = run_command_tool(&env_for(&h.root, SandboxMode::Off, vec![]));
        let r = run(&tool, json!({ "command": "seq 1 5000" })).await;
        let text = text(&r);
        assert!(r.truncated);
        assert!(text.contains("5000\n"));
        assert!(!text.starts_with("1\n"));
        assert!(text.contains("[Output truncated: showing the last 2000 lines"));
        let log = Regex::new(r"Full output \(scratch, outside the workspace\): (\S+)\]")
            .expect("notice pattern")
            .captures(&text)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().to_owned())
            .expect("full log path in notice");
        let full = tokio::fs::read_to_string(&log).await.unwrap();
        assert!(full.starts_with("1\n2\n"));
        assert!(full.trim_end().ends_with("5000"));
        assert!(!log.starts_with(h.root.to_str().unwrap()));
        let details = r.data.clone().unwrap();
        assert_eq!(details["fullLog"], json!(log));
    }

    #[tokio::test]
    async fn runs_an_allowed_test_command_exactly_as_configured() {
        let h = TempRoot::new("tests-ok").await;
        let tool = run_tests_tool(&env_for(
            &h.root,
            SandboxMode::Off,
            vec!["echo tests-ok".to_owned()],
        ));
        let r = run(&tool, json!({ "command": "echo tests-ok" })).await;
        assert!(r.ok);
        assert_eq!(text(&r), "tests-ok\n[exit code 0]");
    }

    #[tokio::test]
    async fn refuses_anything_that_is_not_an_exact_configured_command() {
        let h = TempRoot::new("tests-refuse").await;
        let tool = run_tests_tool(&env_for(
            &h.root,
            SandboxMode::Off,
            vec!["echo tests-ok".to_owned()],
        ));
        for command in [
            "echo tests-ok; id",
            "echo tests-ok ",
            "ECHO tests-ok",
            "rm -rf x",
            "bun test",
        ] {
            let r = run(&tool, json!({ "command": command })).await;
            assert!(!r.ok, "{command} must be refused");
            assert!(text(&r).contains("test_command_not_allowed"));
        }
        let none = run_tests_tool(&env_for(&h.root, SandboxMode::Off, vec![]));
        let r = run(&none, json!({ "command": "bun test" })).await;
        assert!(text(&r).contains("(none configured)"));
    }

    #[tokio::test]
    async fn a_failing_test_command_is_reported_with_its_output() {
        let h = TempRoot::new("tests-fail").await;
        let tool = run_tests_tool(&env_for(
            &h.root,
            SandboxMode::Off,
            vec!["echo FAIL; exit 1".to_owned()],
        ));
        let r = run(&tool, json!({ "command": "echo FAIL; exit 1" })).await;
        assert!(!r.ok);
        assert_eq!(text(&r), "FAIL\n[exit code 1]");
    }
}
