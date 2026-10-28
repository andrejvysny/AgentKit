//! `git_inspect`: read-only git inspection with repository-supplied helpers
//! neutralized. Port of `packages/toolpacks-coding/src/git-tool.ts`.
//!
//! Only `status`, `diff`, `log` and `show` run, from fixed argv; user text
//! only ever fills value slots. Hooks, fsmonitor, filter/textconv/external
//! diff drivers, the pager and ssh/askpass helpers the repository config
//! defines are overridden per driver or disabled outright, and a config that
//! cannot be enumerated fails the call closed.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use agentkit_core::{Tool, ToolContext};
use regex::Regex;
use serde_json::{Map, Value, json};
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

use crate::args::{as_record, opt_bool, opt_int, opt_string, req_string};
use crate::context::{Outcome, PackEnv, ToolEffect, ToolSpec, define_tool};
use crate::errors::CodingError;
use crate::paths::{ConfineOptions, PathAccess, confine, sensitive_git_excludes};
use crate::truncate::format_size;

const COMMANDS: [&str; 4] = ["status", "diff", "log", "show"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GitCommand {
    Status,
    Diff,
    Log,
    Show,
}

impl GitCommand {
    fn as_str(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Diff => "diff",
            Self::Log => "log",
            Self::Show => "show",
        }
    }
}

fn parse_git_command(value: &str) -> Option<GitCommand> {
    match value {
        "status" => Some(GitCommand::Status),
        "diff" => Some(GitCommand::Diff),
        "log" => Some(GitCommand::Log),
        "show" => Some(GitCommand::Show),
        _ => None,
    }
}

static REV_PATTERN: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_./~^-]{1,100}$").expect("rev pattern"));

/// Config that keeps git from running anything the repository supplies. Filter
/// and textconv drivers are named by the repository, so they are neutralized
/// per driver in [`driver_overrides`]; these are the ones with a fixed name.
/// `diff.external=` is inert only together with `--no-ext-diff`, which every
/// diff-capable command below passes.
pub const GIT_SAFETY_ARGS: &[&str] = &[
    "--no-pager",
    "-c",
    "core.hooksPath=/dev/null",
    "-c",
    "core.fsmonitor=false",
    "-c",
    "core.sshCommand=false",
    "-c",
    "core.pager=cat",
    "-c",
    "diff.external=",
    "-c",
    "log.showSignature=false",
];

static DRIVER_KEY: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r"^(filter|diff)\.(.+)\.(clean|smudge|process|textconv|command)$")
        .expect("driver key pattern")
});
const DRIVER_KEY_REGEXP: &str = "^(filter|diff)\\..*\\.(clean|smudge|process|textconv|command)$";

/// JS `String.prototype.slice(0, n)` counts UTF-16 code units.
fn slice_utf16(value: &str, max_units: usize) -> String {
    let units: Vec<u16> = value.encode_utf16().take(max_units).collect();
    String::from_utf16_lossy(&units)
}

/// Rendering of `run.code` inside template strings, where `null` prints as
/// the literal `null`.
fn code_text(code: Option<i32>) -> String {
    code.map(|code| code.to_string())
        .unwrap_or_else(|| "null".to_owned())
}

/// Per-driver overrides for every filter/diff driver the repository config
/// defines. An empty `process` disables the long-running filter (git skips it
/// and falls back to clean/smudge, which are `cat`); an empty `textconv` or
/// `command` is likewise inert once --no-textconv/--no-ext-diff are passed.
pub fn driver_overrides(keys: &[String]) -> Result<Vec<String>, CodingError> {
    let mut out: Vec<String> = vec![];
    let mut seen: HashSet<String> = HashSet::new();
    for key in keys {
        let Some(captures) = DRIVER_KEY.captures(key) else {
            continue;
        };
        let section = captures[1].to_owned();
        let name = captures.get(2).map(|m| m.as_str().to_owned());
        // -c splits on the first "=", so such a name cannot be overridden safely.
        let Some(name) = name.filter(|name| {
            !name.is_empty() && !name.contains('=') && !name.chars().any(|c| (c as u32) < 0x20)
        }) else {
            return Err(CodingError::new(
                "git_config_unreadable",
                "a git driver name in the repository config cannot be neutralized",
            ));
        };
        let id = format!("{section}.{name}");
        if !seen.insert(id) {
            continue;
        }
        if section == "filter" {
            out.push("-c".to_owned());
            out.push(format!("filter.{name}.clean=cat"));
            out.push("-c".to_owned());
            out.push(format!("filter.{name}.smudge=cat"));
            out.push("-c".to_owned());
            out.push(format!("filter.{name}.process="));
            out.push("-c".to_owned());
            out.push(format!("filter.{name}.required=false"));
        } else {
            out.push("-c".to_owned());
            out.push(format!("diff.{name}.textconv="));
            out.push("-c".to_owned());
            out.push(format!("diff.{name}.command="));
        }
    }
    Ok(out)
}

#[derive(Clone, Debug)]
pub struct GitInvocation {
    pub command: GitCommand,
    pub path: Option<String>,
    pub staged: Option<bool>,
    pub max_count: Option<i64>,
    pub rev: Option<String>,
}

/// Fixed argv per command; user text only ever fills a value slot.
pub fn build_git_args(
    root: &str,
    call: &GitInvocation,
    exclude_sensitive: bool,
    overrides: &[String],
) -> Vec<String> {
    let mut args = vec!["-C".to_owned(), root.to_owned()];
    args.extend(GIT_SAFETY_ARGS.iter().map(|arg| (*arg).to_owned()));
    args.extend(overrides.iter().cloned());
    let pathspec = || -> Vec<String> {
        let tail = if call.path.is_some() || exclude_sensitive {
            vec![
                "--".to_owned(),
                call.path.clone().unwrap_or_else(|| ".".to_owned()),
            ]
        } else {
            vec![]
        };
        if exclude_sensitive && !tail.is_empty() {
            tail.into_iter().chain(sensitive_git_excludes()).collect()
        } else {
            tail
        }
    };
    match call.command {
        GitCommand::Status => {
            args.push("status".to_owned());
            args.push("--porcelain=v1".to_owned());
            args.push("--branch".to_owned());
            args.extend(pathspec());
        }
        GitCommand::Diff => {
            args.push("diff".to_owned());
            args.push("--no-color".to_owned());
            args.push("--no-ext-diff".to_owned());
            args.push("--no-textconv".to_owned());
            if call.staged == Some(true) {
                args.push("--cached".to_owned());
            }
            if let Some(rev) = &call.rev {
                args.push(rev.clone());
            }
            args.extend(pathspec());
        }
        GitCommand::Log => {
            args.push("log".to_owned());
            args.push("--no-color".to_owned());
            args.push("--date=short".to_owned());
            args.push("--no-ext-diff".to_owned());
            args.push("--no-textconv".to_owned());
            args.push("--format=%h %ad %an %s".to_owned());
            args.push(format!("--max-count={}", call.max_count.unwrap_or(20)));
            if let Some(rev) = &call.rev {
                args.push(rev.clone());
            }
            if let Some(path) = &call.path {
                args.push("--".to_owned());
                args.push(path.clone());
            }
        }
        GitCommand::Show => {
            args.push("show".to_owned());
            args.push("--no-color".to_owned());
            args.push("--no-ext-diff".to_owned());
            args.push("--no-textconv".to_owned());
            args.push(call.rev.clone().unwrap_or_else(|| "HEAD".to_owned()));
            args.extend(pathspec());
        }
    }
    args
}

struct GitRun {
    stdout: String,
    stderr: String,
    code: Option<i32>,
    capped: bool,
    timed_out: bool,
}

/// Fixed env: no user config, no prompts, no pager, no external helpers.
fn git_env() -> Vec<(String, String)> {
    [("PATH", ""), ("HOME", ""), ("LANG", "C")]
        .into_iter()
        .map(|(key, fallback)| {
            (
                key.to_owned(),
                std::env::var(key).unwrap_or_else(|_| fallback.to_owned()),
            )
        })
        .chain([
            ("GIT_TERMINAL_PROMPT".to_owned(), "0".to_owned()),
            ("GIT_OPTIONAL_LOCKS".to_owned(), "0".to_owned()),
            ("GIT_CONFIG_NOSYSTEM".to_owned(), "1".to_owned()),
            ("GIT_CONFIG_GLOBAL".to_owned(), "/dev/null".to_owned()),
            ("GIT_PAGER".to_owned(), "cat".to_owned()),
            ("GIT_EXTERNAL_DIFF".to_owned(), String::new()),
            ("GIT_ASKPASS".to_owned(), "false".to_owned()),
            ("SSH_ASKPASS".to_owned(), "false".to_owned()),
        ])
        .collect()
}

/// Read a stream up to `max_bytes`; past the cap, kill the child the way
/// `execFile`'s `maxBuffer` does and report the overflow.
async fn read_capped<R: AsyncReadExt + Unpin>(
    mut stream: R,
    max_bytes: usize,
    pid: i32,
) -> (Vec<u8>, bool) {
    let mut collected: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 16384];
    loop {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                collected.extend_from_slice(&chunk[..read]);
                if collected.len() > max_bytes {
                    unsafe { libc::kill(pid, libc::SIGKILL) };
                    return (collected, true);
                }
            }
        }
    }
    (collected, false)
}

async fn read_opt<S: AsyncReadExt + Unpin>(
    stream: Option<S>,
    max_bytes: usize,
    pid: i32,
) -> (Vec<u8>, bool) {
    match stream {
        Some(stream) => read_capped(stream, max_bytes, pid).await,
        None => (vec![], false),
    }
}

/// `execFile("git", args, { maxBuffer: cap * 4, timeout, signal, env })`.
async fn run_git(
    args: &[String],
    cap: usize,
    timeout_ms: u64,
    cancellation: Option<CancellationToken>,
) -> GitRun {
    let max_bytes = cap.saturating_mul(4);
    let spawned = tokio::process::Command::new("git")
        .args(args)
        .env_clear()
        .envs(git_env())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn();
    let mut child = match spawned {
        // execFile with an unspawnable program: the error code is a string,
        // which maps to `null`.
        Err(_) => {
            return GitRun {
                stdout: String::new(),
                stderr: String::new(),
                code: None,
                capped: false,
                timed_out: false,
            };
        }
        Ok(child) => child,
    };
    let pid = child.id().unwrap_or(0) as i32;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let work = async move {
        let ((out, out_capped), (err, err_capped)) = futures::future::join(
            read_opt(stdout, max_bytes, pid),
            read_opt(stderr, max_bytes, pid),
        )
        .await;
        let status = child.wait().await.ok();
        (out, out_capped, err, err_capped, status)
    };
    let timed_out = async {
        tokio::time::sleep(Duration::from_millis(timeout_ms)).await;
        unsafe { libc::kill(pid, libc::SIGKILL) };
    };
    let aborted = async {
        match &cancellation {
            Some(token) => token.cancelled().await,
            None => std::future::pending::<()>().await,
        }
    };
    tokio::select! {
        result = work => {
            let (out, out_capped, err, err_capped, status) = result;
            let capped = out_capped || err_capped;
            GitRun {
                stdout: String::from_utf8_lossy(&out).into_owned(),
                stderr: String::from_utf8_lossy(&err).into_owned(),
                // execFile reports ERR_CHILD_PROCESS_STDIO_MAXBUFFER, a
                // string code, which maps to `null`.
                code: if capped { None } else { status.and_then(|status| status.code()) },
                capped,
                timed_out: false,
            }
        }
        _ = timed_out => GitRun {
            stdout: String::new(),
            stderr: String::new(),
            code: None,
            capped: false,
            timed_out: true,
        },
        // execFile aborts with ABORT_ERR: not timed out, no numeric code.
        _ = aborted => GitRun {
            stdout: String::new(),
            stderr: String::new(),
            code: None,
            capped: false,
            timed_out: false,
        },
    }
}

/// Names of every filter/diff driver the repository config (includes
/// honored) defines. Fails closed: if git cannot list them, nothing runs.
async fn configured_drivers(
    root: &str,
    timeout_ms: u64,
    cancellation: Option<CancellationToken>,
) -> Result<Vec<String>, CodingError> {
    let mut args = vec!["-C".to_owned(), root.to_owned()];
    args.extend(GIT_SAFETY_ARGS.iter().map(|arg| (*arg).to_owned()));
    args.extend(
        [
            "config",
            "-z",
            "--name-only",
            "--get-regexp",
            DRIVER_KEY_REGEXP,
        ]
        .map(str::to_owned),
    );
    let run = run_git(&args, 1 << 20, timeout_ms, cancellation).await;
    // Exit 1 means "no matching key"; anything else is an unreadable config.
    if run.timed_out || run.capped || !matches!(run.code, Some(0) | Some(1)) {
        let shown = slice_utf16(run.stderr.trim(), 300);
        return Err(CodingError::new(
            "git_config_unreadable",
            format!(
                "could not enumerate git drivers: {}",
                if shown.is_empty() {
                    format!("exit {}", code_text(run.code))
                } else {
                    shown
                }
            ),
        ));
    }
    Ok(run
        .stdout
        .split('\0')
        .filter(|key| !key.is_empty())
        .map(|key| key.to_owned())
        .collect())
}

pub fn git_inspect_tool(env: &Arc<PackEnv>) -> Arc<dyn Tool> {
    let l = env.limits.clone();
    let allow_sensitive = env.allow_sensitive;
    let description = format!(
        "Read-only git inspection: status, diff (staged or not), log, show. Fixed commands only. Hooks, fsmonitor, filter/textconv/external-diff drivers, pager and ssh/askpass helpers configured by the repository are neutralized and never run; if the repository config cannot be read the call is refused. Output is capped at {}. Prefer this over running git through a shell.",
        format_size(l.git_max_output_bytes)
    );
    let input_schema = json!({
        "type": "object",
        "properties": {
            "command": { "type": "string", "enum": COMMANDS },
            "args": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Limit to this path (relative to the workspace root)."
                    },
                    "staged": {
                        "type": "boolean",
                        "description": "diff only: show staged changes."
                    },
                    "maxCount": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": l.git_max_count,
                        "description": "log only."
                    },
                    "rev": {
                        "type": "string",
                        "maxLength": 100,
                        "description": "Commit, branch, tag or range, e.g. HEAD~2."
                    }
                },
                "additionalProperties": false
            }
        },
        "required": ["command"],
        "additionalProperties": false
    });
    let env = Arc::clone(env);
    define_tool(
        ToolSpec {
            name: "git_inspect",
            effect: ToolEffect::Read,
            capability: "git.read",
            description,
            input_schema,
            timeout_ms: l.git_timeout_ms + 5000,
        },
        move |ctx: ToolContext, raw: Value| {
            let env = Arc::clone(&env);
            let l = l.clone();
            async move {
                let input = as_record(&raw)?;
                let command_text = req_string(input, "command", 16)?;
                let Some(command) = parse_git_command(&command_text) else {
                    return Err(CodingError::new(
                        "git_command_not_allowed",
                        format!(
                            "git command \"{}\" is not allowed; use one of {}",
                            slice_utf16(&command_text, 40),
                            COMMANDS.join(", ")
                        ),
                    ));
                };
                let args = match input.get("args") {
                    None => Map::new(),
                    Some(args) => as_record(args)?.clone(),
                };
                let rev = opt_string(&args, "rev", 100)?;
                if rev
                    .as_ref()
                    .is_some_and(|rev| rev.starts_with('-') || !REV_PATTERN.is_match(rev))
                {
                    return Err(CodingError::new(
                        "invalid_rev",
                        "rev must match [A-Za-z0-9_./~^-]{1,100} and not start with -",
                    ));
                }
                let root = env.root().await?;
                let raw_path = opt_string(&args, "path", 4096)?;
                let target = match raw_path {
                    None => None,
                    Some(raw_path) => Some(
                        confine(
                            &root,
                            &raw_path,
                            ConfineOptions {
                                access: PathAccess::Read,
                                allow_sensitive: env.allow_sensitive,
                            },
                        )
                        .await?,
                    ),
                };
                // A path inside .git is metadata, not source: only the fixed
                // commands read it.
                let call = GitInvocation {
                    command,
                    path: target.as_ref().map(|target| {
                        if target.relative_path.is_empty() {
                            ".".to_owned()
                        } else {
                            target.relative_path.clone()
                        }
                    }),
                    staged: opt_bool(&args, "staged")?,
                    max_count: opt_int(&args, "maxCount", 1, l.git_max_count as i64)?,
                    rev,
                };
                let drivers =
                    configured_drivers(&root, l.git_timeout_ms, Some(ctx.cancellation.clone()))
                        .await?;
                if ctx.cancellation.is_cancelled() {
                    return Err(CodingError::new("aborted", "the call was aborted"));
                }
                let overrides = driver_overrides(&drivers)?;
                let run = run_git(
                    &build_git_args(&root, &call, !allow_sensitive, &overrides),
                    l.git_max_output_bytes,
                    l.git_timeout_ms,
                    Some(ctx.cancellation.clone()),
                )
                .await;
                if ctx.cancellation.is_cancelled() {
                    return Err(CodingError::new("aborted", "the call was aborted"));
                }
                if run.timed_out {
                    return Err(CodingError::new(
                        "git_timeout",
                        format!("git {} timed out", command.as_str()),
                    ));
                }
                if !run.capped && run.code != Some(0) {
                    let source = if !run.stderr.is_empty() {
                        &run.stderr
                    } else {
                        &run.stdout
                    };
                    let shown = slice_utf16(
                        source.trim_matches(crate::edit_engine::js_trim_matches),
                        2000,
                    );
                    return Ok(Outcome {
                        ok: false,
                        text: format!(
                            "git_failed: {}",
                            if shown.is_empty() {
                                format!("exit {}", code_text(run.code))
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
                let bytes = run.stdout.as_bytes();
                let over = run.capped || bytes.len() > l.git_max_output_bytes;
                let text = if over {
                    format!(
                        "{}\n[output truncated at {}; narrow with args.path or args.rev]",
                        String::from_utf8_lossy(&bytes[..bytes.len().min(l.git_max_output_bytes)]),
                        format_size(l.git_max_output_bytes)
                    )
                } else {
                    run.stdout.clone()
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
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::default_in_process_coordinator;
    use crate::limits::CodingLimits;
    use crate::types::{SandboxMode, system_clock};
    use agentkit_core::{ToolLimits, ToolResult};
    use std::path::{Path, PathBuf};

    struct TempRoot {
        root: PathBuf,
    }

    impl TempRoot {
        async fn new(tag: &str) -> Self {
            let root =
                std::env::temp_dir().join(format!("ak-git-tool-{tag}-{}", uuid::Uuid::new_v4()));
            tokio::fs::create_dir_all(&root).await.unwrap();
            let root = tokio::fs::canonicalize(&root).await.unwrap();
            Self { root }
        }

        fn path(&self) -> &Path {
            &self.root
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

    fn git_available() -> bool {
        std::process::Command::new("git")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    fn git(root: &Path, extra: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args([
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@example.com",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(extra)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {extra:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn env_with(root: &Path, limits: CodingLimits) -> Arc<PackEnv> {
        Arc::new(PackEnv::new(crate::context::EnvInit {
            workspace_root: root.to_string_lossy().into_owned(),
            limits,
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

    /// `harness()` + the TS `repo()` fixture: a committed `a.txt` and a
    /// force-added `.env` on `main`.
    async fn repo(tag: &str) -> TempRoot {
        let h = TempRoot::new(tag).await;
        git(h.path(), &["init", "-q", "-b", "main"]);
        h.put("a.txt", "one\ntwo\n").await;
        h.put(".env", "SECRET=1\n").await;
        git(h.path(), &["add", "-f", "a.txt", ".env"]);
        git(h.path(), &["commit", "-q", "-m", "first commit"]);
        h
    }

    fn call(command: GitCommand) -> GitInvocation {
        GitInvocation {
            command,
            path: None,
            staged: None,
            max_count: None,
            rev: None,
        }
    }

    #[test]
    fn the_argv_is_fixed_hook_free_and_shell_free() {
        let args = build_git_args(
            "/r",
            &GitInvocation {
                command: GitCommand::Diff,
                path: Some("src".to_owned()),
                staged: Some(true),
                max_count: None,
                rev: Some("HEAD~1".to_owned()),
            },
            true,
            &[],
        );
        assert_eq!(&args[..2], ["-C", "/r"]);
        assert_eq!(&args[2..2 + GIT_SAFETY_ARGS.len()], GIT_SAFETY_ARGS);
        assert!(GIT_SAFETY_ARGS.contains(&"core.hooksPath=/dev/null"));
        assert!(GIT_SAFETY_ARGS.contains(&"core.fsmonitor=false"));
        assert!(GIT_SAFETY_ARGS.contains(&"core.sshCommand=false"));
        assert!(args.contains(&"--no-ext-diff".to_owned()));
        assert!(args.contains(&"--no-textconv".to_owned()));
        assert!(
            args.iter().rposition(|a| a == "--").unwrap()
                > args.iter().position(|a| a == "HEAD~1").unwrap()
        );
        assert!(args.contains(&"src".to_owned()));
        // Sensitive excludes trail the pathspec.
        assert!(args.iter().any(|a| a.starts_with(":(exclude,glob)**/.env")));
    }

    #[test]
    fn argv_matrix_per_command() {
        // status, no path, no excludes: ends at --branch.
        let args = build_git_args("/r", &call(GitCommand::Status), false, &[]);
        assert_eq!(
            &args[2 + GIT_SAFETY_ARGS.len()..],
            ["status", "--porcelain=v1", "--branch"]
        );
        // status with excludes: `-- .` plus every exclude glob.
        let args = build_git_args("/r", &call(GitCommand::Status), true, &[]);
        let tail = &args[2 + GIT_SAFETY_ARGS.len()..];
        assert_eq!(
            &tail[..5],
            ["status", "--porcelain=v1", "--branch", "--", "."]
        );
        assert!(tail.iter().any(|a| a.starts_with(":(exclude,glob)")));
        // diff without path or excludes carries no `--`.
        let args = build_git_args("/r", &call(GitCommand::Diff), false, &[]);
        assert!(!args.iter().any(|a| a == "--"));
        // log clamps through max-count, keeps its own `-- path` and never
        // appends the sensitive excludes.
        let args = build_git_args(
            "/r",
            &GitInvocation {
                command: GitCommand::Log,
                path: Some("a.txt".to_owned()),
                staged: None,
                max_count: Some(5),
                rev: Some("HEAD~2".to_owned()),
            },
            true,
            &[],
        );
        assert!(args.contains(&"--max-count=5".to_owned()));
        assert!(args.contains(&"--format=%h %ad %an %s".to_owned()));
        let index = args.iter().position(|a| a == "HEAD~2").unwrap();
        assert_eq!(&args[index + 1..], ["--", "a.txt"]);
        assert!(!args.iter().any(|a| a.starts_with(":(exclude")));
        // log defaults to max-count 20.
        let args = build_git_args("/r", &call(GitCommand::Log), false, &[]);
        assert!(args.contains(&"--max-count=20".to_owned()));
        // show defaults to HEAD.
        let args = build_git_args("/r", &call(GitCommand::Show), false, &[]);
        assert!(args.contains(&"HEAD".to_owned()));
        // overrides sit between the safety args and the subcommand.
        let overrides = vec!["-c".to_owned(), "filter.x.clean=cat".to_owned()];
        let args = build_git_args("/r", &call(GitCommand::Status), false, &overrides);
        assert_eq!(
            &args[2 + GIT_SAFETY_ARGS.len()..2 + GIT_SAFETY_ARGS.len() + 3],
            ["-c", "filter.x.clean=cat", "status"]
        );
    }

    #[test]
    fn driver_overrides_neutralize_every_driver_once() {
        let keys = [
            "filter.x.clean",
            "filter.x.smudge",
            "diff.y.textconv",
            "diff.y.command",
            "core.editor",
            "filter.a.b.process",
        ]
        .map(str::to_owned)
        .to_vec();
        assert_eq!(
            driver_overrides(&keys).unwrap(),
            [
                "-c",
                "filter.x.clean=cat",
                "-c",
                "filter.x.smudge=cat",
                "-c",
                "filter.x.process=",
                "-c",
                "filter.x.required=false",
                "-c",
                "diff.y.textconv=",
                "-c",
                "diff.y.command=",
                "-c",
                "filter.a.b.clean=cat",
                "-c",
                "filter.a.b.smudge=cat",
                "-c",
                "filter.a.b.process=",
                "-c",
                "filter.a.b.required=false",
            ]
        );
    }

    #[test]
    fn driver_overrides_rejects_unneutralizable_names() {
        let keys = ["filter.a=1.clean".to_owned()];
        assert_eq!(
            driver_overrides(&keys).unwrap_err().code,
            "git_config_unreadable"
        );
        let keys = ["filter.a\tb.smudge".to_owned()];
        let error = driver_overrides(&keys).unwrap_err();
        assert_eq!(error.code, "git_config_unreadable");
        assert_eq!(
            error.message,
            "a git driver name in the repository config cannot be neutralized"
        );
        // Keys that are not drivers are ignored, not rejected.
        assert!(
            driver_overrides(&["core.editor".to_owned()])
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn any_other_command_or_argument_shape_is_rejected() {
        let h = TempRoot::new("allowlist").await;
        let tool = git_inspect_tool(&env_with(h.path(), CodingLimits::default()));
        for command in [
            "push",
            "checkout",
            "config",
            "reset",
            "log --all",
            "status;id",
            "-h",
        ] {
            let r = run(&tool, json!({ "command": command })).await;
            assert!(!r.ok, "{command} must be refused");
            assert!(text(&r).contains("git_command_not_allowed"));
            assert!(text(&r).contains("use one of status, diff, log, show"));
        }
        let r = run(
            &tool,
            json!({ "command": "log", "args": { "maxCount": 51 } }),
        )
        .await;
        assert!(!r.ok);
        assert!(text(&r).contains("invalid_arguments"));
        let r = run(&tool, json!({})).await;
        assert!(!r.ok);
        assert!(text(&r).contains("\"command\" must be a non-empty string"));
    }

    #[tokio::test]
    async fn a_bad_rev_is_rejected_including_option_shaped_ones() {
        let h = TempRoot::new("rev").await;
        let tool = git_inspect_tool(&env_with(h.path(), CodingLimits::default()));
        for rev in [
            "--output=/tmp/x",
            "-p",
            "a b",
            "x;rm",
            "HEAD:.env",
            &"a".repeat(101),
            "$(id)",
            "`id`",
        ] {
            let r = run(&tool, json!({ "command": "show", "args": { "rev": rev } })).await;
            assert!(!r.ok, "{rev} must be refused");
            let text = text(&r);
            assert!(
                text.contains("invalid_rev") || text.contains("invalid_arguments"),
                "{rev}: {text}"
            );
        }
    }

    #[tokio::test]
    async fn status_diff_log_and_show_in_a_temp_repo() {
        if !git_available() {
            eprintln!("git not installed; skipping");
            return;
        }
        let h = repo("smoke").await;
        h.put("a.txt", "one\nTWO\n").await;
        h.put("new.txt", "n\n").await;
        let env = env_with(h.path(), CodingLimits::default());
        let tool = git_inspect_tool(&env);
        let status = text(&run(&tool, json!({ "command": "status" })).await);
        assert!(status.contains("## main"));
        assert!(status.contains(" M a.txt"));
        assert!(status.contains("?? new.txt"));
        let diff = text(&run(&tool, json!({ "command": "diff" })).await);
        assert!(diff.contains("-two"));
        assert!(diff.contains("+TWO"));
        git(h.path(), &["add", "a.txt"]);
        assert_eq!(
            text(&run(&tool, json!({ "command": "diff" })).await),
            "(no output)"
        );
        let staged = text(
            &run(
                &tool,
                json!({ "command": "diff", "args": { "staged": true } }),
            )
            .await,
        );
        assert!(staged.contains("+TWO"));
        let log = text(
            &run(
                &tool,
                json!({ "command": "log", "args": { "maxCount": 5 } }),
            )
            .await,
        );
        assert!(
            Regex::new(r"^[0-9a-f]{7,40} \d{4}-\d{2}-\d{2} T first commit$")
                .unwrap()
                .is_match(log.trim()),
            "unexpected log line: {log}"
        );
        let show = text(
            &run(
                &tool,
                json!({ "command": "show", "args": { "rev": "HEAD" } }),
            )
            .await,
        );
        assert!(show.contains("first commit"));
        assert!(show.contains("+one"));
    }

    #[tokio::test]
    async fn a_path_argument_narrows_the_output_and_is_confined() {
        if !git_available() {
            eprintln!("git not installed; skipping");
            return;
        }
        let h = repo("path").await;
        h.put("a.txt", "changed\n").await;
        h.put("b.txt", "x\n").await;
        git(h.path(), &["add", "b.txt"]);
        let tool = git_inspect_tool(&env_with(h.path(), CodingLimits::default()));
        let status = text(
            &run(
                &tool,
                json!({ "command": "status", "args": { "path": "b.txt" } }),
            )
            .await,
        );
        assert!(status.contains("A  b.txt"));
        assert!(!status.contains("a.txt"));
        let out = text(
            &run(
                &tool,
                json!({ "command": "diff", "args": { "path": "../x" } }),
            )
            .await,
        );
        assert!(out.contains("path_outside_root"));
        let out = text(
            &run(
                &tool,
                json!({ "command": "show", "args": { "path": ".git/config" } }),
            )
            .await,
        );
        assert!(out.contains("path_sensitive"));
    }

    #[tokio::test]
    async fn changes_to_sensitive_tracked_files_never_appear() {
        if !git_available() {
            eprintln!("git not installed; skipping");
            return;
        }
        let h = repo("sensitive").await;
        h.put(".env", "SECRET=changed-secret\n").await;
        h.put("a.txt", "one\nTWO\n").await;
        let tool = git_inspect_tool(&env_with(h.path(), CodingLimits::default()));
        let diff = text(&run(&tool, json!({ "command": "diff" })).await);
        assert!(diff.contains("+TWO"));
        assert!(!diff.contains("changed-secret"));
        let status = text(&run(&tool, json!({ "command": "status" })).await);
        assert!(!status.contains(".env"));
        let show = text(&run(&tool, json!({ "command": "show" })).await);
        assert!(!show.contains("SECRET=1"));
    }

    #[tokio::test]
    async fn an_unreadable_config_fails_closed() {
        if !git_available() {
            eprintln!("git not installed; skipping");
            return;
        }
        let h = repo("broken-config").await;
        h.put(".git/config", "[core\n\tbroken = \n").await;
        let r = run(
            &git_inspect_tool(&env_with(h.path(), CodingLimits::default())),
            json!({ "command": "status" }),
        )
        .await;
        assert!(!r.ok);
        assert!(text(&r).contains("git_config_unreadable"));
    }

    #[tokio::test]
    async fn the_output_is_capped_and_says_so() {
        if !git_available() {
            eprintln!("git not installed; skipping");
            return;
        }
        let h = TempRoot::new("cap").await;
        git(h.path(), &["init", "-q", "-b", "main"]);
        h.put("a.txt", "x\n").await;
        git(h.path(), &["add", "a.txt"]);
        git(h.path(), &["commit", "-q", "-m", "c"]);
        let body = (0..200)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        h.put("a.txt", &body).await;
        let limits = CodingLimits {
            git_max_output_bytes: 300,
            ..CodingLimits::default()
        };
        let r = run(
            &git_inspect_tool(&env_with(h.path(), limits)),
            json!({ "command": "diff" }),
        )
        .await;
        assert!(r.truncated);
        assert!(text(&r).contains("[output truncated at 300B"));
    }

    #[tokio::test]
    async fn a_directory_that_is_not_a_repository_fails_with_a_message() {
        if !git_available() {
            eprintln!("git not installed; skipping");
            return;
        }
        let h = TempRoot::new("norepo").await;
        let r = run(
            &git_inspect_tool(&env_with(h.path(), CodingLimits::default())),
            json!({ "command": "status" }),
        )
        .await;
        assert!(!r.ok);
        assert!(text(&r).contains("git_failed"));
    }
}
