//! Shell command classification and the coding pack's standing-approval
//! policy. Port of `packages/toolpacks-coding/src/command-policy.ts`.
//!
//! A standing approval lets a human say "yes, this exact command, for the
//! rest of this session". That is only safe for commands whose effect is
//! bounded by what they are, so eligibility is a POSITIVE allowlist over a
//! character set with no shell syntax in it: a command is `eligible` only
//! when every character is plain and its tokens spell one of a short list
//! of build/test/inspect invocations. Everything else is `unknown` and asks
//! the human each time. `high_risk` is a separate, earlier verdict used by
//! the guard to force a fresh approval even in `full_auto`.
//!
//! Divergences (tracked): the Rust host has not ported the ADR 0019
//! standing-approval ledger yet, so [`coding_standing_approval_key`] is
//! exposed as a pure function for hosts that implement their own approval
//! ledger. The high-risk guard is a real [`ToolGuard`] ([`HighRiskCommandGuard`])
//! now that the host passes per-call `arguments` to `canExecute`.

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

/// Ported classification of `packages/toolpacks-coding/src/command-policy.ts`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandClass {
    Eligible,
    HighRisk,
    Unknown,
}

impl CommandClass {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Eligible => "eligible",
            Self::HighRisk => "high_risk",
            Self::Unknown => "unknown",
        }
    }
}

fn shell_tools() -> &'static [&'static str] {
    &["run_command", "run_tests"]
}

/// True for the tool names this policy covers.
pub fn is_shell_tool(name: &str) -> bool {
    shell_tools().contains(&name)
}

fn high_risk_words() -> &'static [&'static str] {
    &[
        "rm",
        "rmdir",
        "unlink",
        "shred",
        "sudo",
        "su",
        "doas",
        "dd",
        "mkfs",
        "diskutil",
        "chmod",
        "chown",
        "chflags",
        "curl",
        "wget",
        "ssh",
        "scp",
        "sftp",
        "rsync",
        "ftp",
        "nc",
        "ncat",
        "netcat",
        "telnet",
        "kill",
        "killall",
        "pkill",
        "shutdown",
        "reboot",
        "launchctl",
        "docker",
        "podman",
        "kubectl",
        "helm",
        "terraform",
        "pulumi",
        "ansible",
        "eval",
        "exec",
        "xargs",
        "osascript",
        "open",
        "crontab",
        "security",
        "defaults",
        "gh",
        "twine",
        "vercel",
        "netlify",
        "fly",
        "flyctl",
        "heroku",
        "firebase",
        "aws",
        "gcloud",
        "az",
    ]
}

fn git_risky_subcommands() -> &'static [&'static str] {
    &[
        "push",
        "reset",
        "clean",
        "rebase",
        "checkout",
        "switch",
        "restore",
        "filter-branch",
        "filter-repo",
        "stash",
        "gc",
        "prune",
        "update-ref",
        "reflog",
        "rm",
        "mv",
        "commit",
        "merge",
        "cherry-pick",
        "revert",
        "am",
        "apply",
        "tag",
        "branch",
        "worktree",
        "submodule",
        "config",
        "remote",
        "fetch",
        "pull",
        "clone",
    ]
}

fn package_managers() -> &'static [&'static str] {
    &["npm", "bun", "yarn", "pnpm", "cargo"]
}

fn publish_subcommands() -> &'static [&'static str] {
    &[
        "publish",
        "unpublish",
        "deprecate",
        "login",
        "logout",
        "adduser",
        "owner",
        "dist-tag",
    ]
}

fn force_flags() -> &'static [&'static str] {
    &["--force", "--hard", "--no-verify"]
}

fn dangerous_word_alternation() -> &'static str {
    "(deploy|publish|release|migrate|migration|seed|drop|destroy|purge|wipe|prod|production)"
}

fn compiled(pattern: &str) -> regex::Regex {
    regex::Regex::new(pattern).expect("static pattern compiles")
}

fn is_high_risk(raw: &str) -> bool {
    static TOKEN_SPLIT: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| compiled(r"[\s;&|()<>\x60$]+"));
    static DANGEROUS: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::RegexBuilder::new(&format!(
            r"(^|[:_-]){}(([:_-])|$)",
            dangerous_word_alternation()
        ))
        .case_insensitive(true)
        .build()
        .expect("static pattern compiles")
    });
    let tokens: Vec<&str> = TOKEN_SPLIT
        .split(raw)
        .filter(|token| !token.is_empty())
        .collect();
    // `find` is high risk only with an action flag, so it is judged per
    // command.
    let has_find = tokens.contains(&"find");
    for (index, token) in tokens.iter().enumerate() {
        if high_risk_words().contains(token) {
            return true;
        }
        // A flag names a build option (`--release`), not a target; only its
        // value can.
        let word = if token.starts_with('-') {
            token.split('=').nth(1).unwrap_or("")
        } else {
            token
        };
        if DANGEROUS.is_match(word) {
            return true;
        }
        if force_flags().contains(token) {
            return true;
        }
        if *token == "git" && tokens[index + 1..].iter().any(|t| is_risky_git_word(t)) {
            return true;
        }
        if package_managers().contains(token)
            && tokens[index + 1..]
                .iter()
                .any(|t| publish_subcommands().contains(t))
        {
            return true;
        }
        if *token == "-f" && tokens[..index].iter().any(|t| is_force_host(t)) {
            return true;
        }
    }
    has_find
        && tokens
            .iter()
            .any(|t| *t == "-delete" || *t == "-exec" || *t == "-execdir")
}

fn is_risky_git_word(token: &str) -> bool {
    git_risky_subcommands().contains(&token)
}

/// `-f` is forceful after git or rm (rm is already high risk by itself).
fn is_force_host(token: &str) -> bool {
    token == "git" || token == "rm"
}

fn starts_with(tokens: &[&str], prefix: &[&str]) -> bool {
    prefix
        .iter()
        .enumerate()
        .all(|(i, word)| tokens.get(i) == Some(word))
}

fn matches_allowlist(tokens: &[&str]) -> bool {
    let a = tokens.first().copied();
    let b = tokens.get(1).copied();
    let c = tokens.get(2).copied();
    if starts_with(tokens, &["bun", "test"]) {
        return true;
    }
    if matches!(a, Some("bun" | "npm" | "pnpm" | "yarn")) && b == Some("run") {
        return tokens.len() == 3 && safe_script().is_match(c.unwrap_or(""));
    }
    if matches!(a, Some("npm" | "pnpm" | "yarn")) && b == Some("test") {
        return tokens.len() == 2;
    }
    if a == Some("tsc") || (matches!(a, Some("npx" | "bunx")) && b == Some("tsc")) {
        return true;
    }
    if a == Some("vitest") && b == Some("run") {
        return true;
    }
    if a == Some("npx") && b == Some("vitest") && c == Some("run") {
        return true;
    }
    if a == Some("jest") || (a == Some("npx") && b == Some("jest")) {
        return true;
    }
    if a == Some("eslint") || (a == Some("npx") && b == Some("eslint")) {
        return true;
    }
    if a == Some("npx") && b == Some("prettier") && c == Some("--check") {
        return true;
    }
    if a == Some("pytest") {
        return true;
    }
    if matches!(a, Some("python" | "python3"))
        && b == Some("-m")
        && matches!(c, Some("pytest") | Some("unittest"))
    {
        return true;
    }
    if a == Some("uv") && b == Some("run") && c == Some("pytest") {
        return true;
    }
    if a == Some("cargo") && matches!(b, Some("test" | "check" | "build" | "clippy")) {
        return true;
    }
    if a == Some("cargo") && b == Some("fmt") && c == Some("--check") {
        return true;
    }
    if a == Some("go") && matches!(b, Some("test" | "build" | "vet")) {
        return true;
    }
    if a == Some("make")
        && tokens.len() == 2
        && matches!(b, Some("test" | "check" | "lint" | "build" | "typecheck"))
    {
        return true;
    }
    if a == Some("git") && matches!(b, Some("status" | "diff" | "log" | "show")) {
        return true;
    }
    false
}

fn safe_script() -> &'static regex::Regex {
    static SAFE_SCRIPT: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        compiled(
            r"\A(test|lint|typecheck|type-check|check|build|format:check|fmt:check|(test|lint|typecheck|check):[A-Za-z0-9_.-]+)\z",
        )
    });
    &SAFE_SCRIPT
}

/// `high_risk` is checked first and on the raw text, metacharacters
/// included, so `echo ok; rm -rf x` is high risk and not merely "unknown".
/// `eligible` needs the full allowlist; there is no "looks read-only" path
/// to it.
pub fn classify_shell_command(command: &str) -> CommandClass {
    static PLAIN_CHARS: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| compiled(r"\A[A-Za-z0-9_./:=@%+, -]+\z"));
    static ASSIGNMENT: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| compiled(r"\A[A-Za-z_][A-Za-z0-9_]*="));
    if is_high_risk(command) {
        return CommandClass::HighRisk;
    }
    if !PLAIN_CHARS.is_match(command) {
        return CommandClass::Unknown;
    }
    let tokens: Vec<&str> = command.split(' ').filter(|t| !t.is_empty()).collect();
    let Some(first) = tokens.first() else {
        return CommandClass::Unknown;
    };
    // A leading VAR=value changes the environment the command runs in.
    if ASSIGNMENT.is_match(first) {
        return CommandClass::Unknown;
    }
    if matches_allowlist(&tokens) {
        CommandClass::Eligible
    } else {
        CommandClass::Unknown
    }
}

/// The reason text a high-risk refusal carries. Enforced only through
/// [`HighRiskCommandGuard`], which the host must register alongside the
/// permission guard; the exec tools themselves never classify (same wiring
/// as TS).
pub fn high_risk_refusal(tool_name: &str) -> String {
    format!("high_risk_command: {tool_name} needs a fresh approval")
}

/// Port of `createHighRiskCommandGuard`: forces a fresh approval for
/// high-risk shell commands even in `full_auto`. Judged per call, on the
/// live arguments; anything outside the coding namespace or off the shell
/// tool list gets no opinion.
pub struct HighRiskCommandGuard;

#[async_trait::async_trait]
impl agentkit_host::ToolGuard for HighRiskCommandGuard {
    async fn can_execute(
        &self,
        context: &agentkit_host::ToolGuardContext,
    ) -> agentkit_host::HostResult<agentkit_host::ToolGuardVerdict> {
        use agentkit_host::ToolGuardVerdict;
        if context.namespace != crate::guard::CODING_NAMESPACE {
            return Ok(ToolGuardVerdict::Allowed);
        }
        let name = context.tool.name.clone();
        if !is_shell_tool(&name) {
            return Ok(ToolGuardVerdict::Allowed);
        }
        let args = context.arguments.as_ref().and_then(shell_args_of);
        let Some(args) = args else {
            return Ok(ToolGuardVerdict::Allowed);
        };
        if classify_shell_command(&args.command) != CommandClass::HighRisk {
            return Ok(ToolGuardVerdict::Allowed);
        }
        // Approvable but uncoverable: the reason does not start with
        // `approval_required:`, so the standing policy never keys it and a
        // high-risk command forces a FRESH human approval in every mode.
        Ok(ToolGuardVerdict::AwaitingHuman(
            agentkit_host::ApprovalAsk {
                reason: high_risk_refusal(&name),
                preview: None,
                requested_mode: None,
                uncertainty_generation: None,
            },
        ))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellArgs {
    pub command: String,
    pub cwd: Option<String>,
}

/// Narrow parsed tool arguments to `{ command, cwd? }` exactly like the TS
/// `shellArgsOf` (a malformed shape is not a refusal, it is uncoverable).
pub fn shell_args_of(args: &Value) -> Option<ShellArgs> {
    let map = args.as_object()?;
    let command = map.get("command")?.as_str()?;
    if command.is_empty() {
        return None;
    }
    let cwd = match map.get("cwd") {
        None => None,
        Some(Value::String(cwd)) => Some(cwd.clone()),
        // TS: `cwd !== undefined && typeof cwd !== "string"` → an explicit
        // JSON null makes the call uncoverable, never a root-cwd key.
        Some(_) => return None,
    };
    // JS checks `typeof command === 'string' && command !== ""`; a non-null
    // object/array passed the `typeof object` gate but not the string gate.
    Some(ShellArgs {
        command: command.to_owned(),
        cwd,
    })
}

/// The workspace a chat's commands run in. `identity` names the environment
/// the approval is valid for (sandbox settings, host, user): change it and
/// every old grant stops matching. `context` is folded in verbatim for
/// anything else the host wants a grant bound to.
#[derive(Clone, Debug)]
pub struct StandingApprovalWorkspace {
    pub root: String,
    pub identity: String,
    pub context: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StandingApprovalKey {
    pub digest: String,
    pub label: String,
}

/// The standing-approval key for one shell call, or `None` when the call is
/// uncoverable (not the coding namespace, not a shell tool, no chat, never
/// eligible, escapes the workspace, cwd missing or not a directory). Port of
/// the TS `keyFor` body minus the host's guard-refusal precondition (which
/// the host ledger must re-check: every refusal must start with
/// `approval_required:`).
pub async fn coding_standing_approval_key(
    namespace: &str,
    chat_id: Option<&str>,
    tool: &str,
    args: &ShellArgs,
    workspace: &StandingApprovalWorkspace,
) -> Option<StandingApprovalKey> {
    // TS `keyFor` preconditions (command-policy.ts): the refusal surface is
    // the coding namespace's two shell tools with a chat; any other call is
    // never keyable and must re-ask every time.
    if namespace != crate::guard::CODING_NAMESPACE || !is_shell_tool(tool) {
        return None;
    }
    let chat_id = chat_id?;
    if classify_shell_command(&args.command) != CommandClass::Eligible {
        return None;
    }
    let root = crate::paths::canonical_root(&workspace.root).await.ok()?;
    let confined = crate::paths::resolve_inside_root(&root, args.cwd.as_deref().unwrap_or("."))
        .await
        .ok()?;
    let cwd = tokio::fs::canonicalize(&confined.path).await.ok()?;
    let cwd = cwd.to_string_lossy().into_owned();
    if !tokio::fs::metadata(&cwd).await.ok()?.is_dir() {
        return None;
    }
    let inside = crate::paths::relative(&root, &cwd);
    if inside == ".." || inside.starts_with("../") || inside.starts_with('/') {
        return None;
    }
    let mut payload = Map::new();
    payload.insert("v".into(), Value::from(1));
    payload.insert("chatId".into(), Value::String(chat_id.to_owned()));
    payload.insert("tool".into(), Value::String(tool.to_owned()));
    payload.insert("identity".into(), Value::String(workspace.identity.clone()));
    payload.insert("root".into(), Value::String(root));
    payload.insert("cwd".into(), Value::String(cwd));
    payload.insert("command".into(), Value::String(args.command.clone()));
    payload.insert(
        "context".into(),
        workspace
            .context
            .clone()
            .map(Value::String)
            .unwrap_or(Value::Null),
    );
    let encoded = serde_json::to_string(&Value::Object(payload)).ok()?;
    let digest = {
        let mut hasher = Sha256::new();
        hasher.update(encoded.as_bytes());
        hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    };
    let label = if inside.is_empty() {
        args.command.clone()
    } else {
        format!("{} (in {inside})", args.command)
    };
    Some(StandingApprovalKey {
        digest,
        // TS `label.slice(0, 512)` counts UTF-16 code units and may cut a
        // surrogate pair; count the same units and let the orphan surrogate
        // become U+FFFD exactly as a re-encoded JS string would.
        label: String::from_utf16_lossy(&label.encode_utf16().take(512).collect::<Vec<_>>()),
    })
}

/// The workspace a chat's commands run in, resolved per chat. `None` means
/// "no workspace": nothing is keyable, every approvable call asks.
#[async_trait::async_trait]
pub trait WorkspaceResolver: Send + Sync {
    async fn resolve(&self, chat_id: &str) -> Option<StandingApprovalWorkspace>;
}

/// Standing approvals for the coding pack's shell tools (port of
/// `createCodingStandingApprovals`). Covers only calls whose EVERY
/// approvable refusal is the ordinary `approval_required:` — an
/// uncertain-outcome, a high-risk, or a plan refusal makes the call
/// uncoverable, because a remembered yes must not erase "this particular
/// call is special".
pub struct CodingStandingApproval<R: WorkspaceResolver> {
    pub resolver: R,
}

#[async_trait::async_trait]
impl<R: WorkspaceResolver> agentkit_host::StandingApprovalPolicy for CodingStandingApproval<R> {
    async fn key_for(
        &self,
        context: &agentkit_host::ToolGuardContext,
        reasons: &[String],
    ) -> agentkit_host::HostResult<Option<(String, String)>> {
        if context.namespace != crate::guard::CODING_NAMESPACE
            || !is_shell_tool(&context.tool.name)
            || reasons.is_empty()
            || !reasons
                .iter()
                .all(|reason| reason.starts_with("approval_required:"))
        {
            return Ok(None);
        }
        let args = context.arguments.as_ref().and_then(shell_args_of);
        let Some(args) = args else {
            return Ok(None);
        };
        let Some(chat_id) = context.contribution.chat_id.as_deref() else {
            return Ok(None);
        };
        let Some(workspace) = self.resolver.resolve(chat_id).await else {
            return Ok(None);
        };
        Ok(coding_standing_approval_key(
            &context.namespace,
            Some(chat_id),
            &context.tool.name,
            &args,
            &workspace,
        )
        .await
        .map(|key| (key.digest, key.label)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ELIGIBLE: &[&str] = &[
        "bun test",
        "bun test packages/host --timeout 5000",
        "bun run test",
        "bun run lint",
        "npm run typecheck",
        "pnpm run check",
        "yarn run build",
        "npm run test:unit",
        "npm run lint:ci",
        "npm run format:check",
        "npm test",
        "pnpm test",
        "yarn test",
        "npx tsc --noEmit",
        "bunx tsc -p tsconfig.json",
        "tsc --noEmit",
        "npx vitest run src/a.test.ts",
        "vitest run",
        "npx jest --ci",
        "jest",
        "npx eslint src",
        "eslint .",
        "npx prettier --check .",
        "pytest -q tests/",
        "python -m pytest tests",
        "python3 -m unittest discover",
        "uv run pytest",
        "cargo test --workspace",
        "cargo check",
        "cargo build",
        "cargo build --release",
        "cargo clippy",
        "cargo fmt --check",
        "go test ./...",
        "go vet ./...",
        "make test",
        "make typecheck",
        "git status",
        "git diff HEAD",
        "git log --oneline -n 5",
        "git show HEAD",
    ];

    const HIGH_RISK: &[&str] = &[
        "rm -rf x",
        "rm x",
        "sudo bun test",
        "curl https://example.com",
        "bun run deploy",
        "npm run build:prod",
        "npm run db:migrate",
        "bun run release",
        "git push",
        "npx vite build --mode=production",
        "git fetch",
        "make deploy-docs",
        "git push --force",
        "git reset --hard",
        "git checkout main",
        "git commit -m x",
        "git stash",
        "git clean -fd",
        "npm publish",
        "bun publish",
        "cargo publish",
        "yarn login",
        "gh pr create",
        "docker run x",
        "find . -name x -delete",
        "find . -exec ls {} ;",
        "echo ok; rm -rf x",
        "bun test && curl x",
        "bun test | xargs echo",
        "echo $(rm x)",
        "bun test --no-verify",
        "bun test --force",
        "kill 1",
        "aws s3 ls",
        "chmod 777 x",
        "eval ls",
    ];

    const UNKNOWN: &[&str] = &[
        "ls",
        "cat README.md",
        "echo hi",
        "bun run dev",
        "bun run test extra",
        "npm run build --silent",
        "npm test -- --watch",
        "npm install",
        "bun test > out.txt",
        "bun test | tee out",
        "bun test; echo done",
        "bun test && echo ok",
        "bun test \"a b\"",
        "bun test 'a'",
        "bun test $HOME",
        "bun test `pwd`",
        "bun test *.ts",
        "bun test ~/x",
        "git diff HEAD~1",
        "bun test\nbun test",
        "bun test\tfoo",
        "bun test #comment",
        "NODE_ENV=test bun test",
        "FOO=1 pytest",
        "make",
        "make test lint",
        "node script.js",
        "python script.py",
        "npx prettier --write .",
        "",
        "   ",
    ];

    #[test]
    fn eligible_commands() {
        for command in ELIGIBLE {
            assert_eq!(
                classify_shell_command(command),
                CommandClass::Eligible,
                "expected eligible: {command:?}"
            );
        }
    }

    #[test]
    fn high_risk_commands() {
        for command in HIGH_RISK {
            assert_eq!(
                classify_shell_command(command),
                CommandClass::HighRisk,
                "expected high_risk: {command:?}"
            );
        }
    }

    #[test]
    fn unknown_commands() {
        for command in UNKNOWN {
            assert_eq!(
                classify_shell_command(command),
                CommandClass::Unknown,
                "expected unknown: {command:?}"
            );
        }
    }

    #[test]
    fn shell_args_narrowing() {
        let ok = serde_json::json!({ "command": "bun test", "cwd": "pkg" });
        assert_eq!(
            shell_args_of(&ok),
            Some(ShellArgs {
                command: "bun test".into(),
                cwd: Some("pkg".into())
            })
        );
        let no_cwd = serde_json::json!({ "command": "bun test" });
        assert_eq!(
            shell_args_of(&no_cwd),
            Some(ShellArgs {
                command: "bun test".into(),
                cwd: None
            })
        );
        for bad in [
            serde_json::json!(null),
            serde_json::json!(["bun test"]),
            serde_json::json!({}),
            serde_json::json!({ "command": "" }),
            serde_json::json!({ "command": 1 }),
            serde_json::json!({ "command": "bun test", "cwd": 5 }),
            // TS: `cwd !== undefined && typeof cwd !== "string"` — JSON null
            // is present-but-not-a-string, so the call is uncoverable.
            serde_json::json!({ "command": "bun test", "cwd": null }),
        ] {
            assert_eq!(shell_args_of(&bad), None, "expected null: {bad}");
        }
    }

    #[tokio::test]
    async fn standing_key_requires_coding_namespace_shell_tool_and_chat() {
        // TS `keyFor` preconditions; anything else re-asks every time.
        let root = std::env::temp_dir().join(format!("ak-cmd-gates-{}", std::process::id()));
        tokio::fs::create_dir_all(&root).await.unwrap();
        let root_str = tokio::fs::canonicalize(&root)
            .await
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let workspace = StandingApprovalWorkspace {
            root: root_str,
            identity: "test-env".into(),
            context: None,
        };
        let args = ShellArgs {
            command: "bun test".into(),
            cwd: None,
        };
        assert_eq!(
            coding_standing_approval_key("other", Some("chat-1"), "run_command", &args, &workspace)
                .await,
            None,
            "foreign namespace is never keyable"
        );
        assert_eq!(
            coding_standing_approval_key(
                crate::guard::CODING_NAMESPACE,
                Some("chat-1"),
                "read_file",
                &args,
                &workspace,
            )
            .await,
            None,
            "a non-shell coding tool is never keyable"
        );
        assert_eq!(
            coding_standing_approval_key(
                crate::guard::CODING_NAMESPACE,
                None,
                "run_command",
                &args,
                &workspace,
            )
            .await,
            None,
            "a chat-less call is never keyable"
        );
        assert!(
            coding_standing_approval_key(
                crate::guard::CODING_NAMESPACE,
                Some("chat-1"),
                "run_command",
                &args,
                &workspace,
            )
            .await
            .is_some(),
            "the un-gated combination still keys"
        );
        let _ = tokio::fs::remove_dir_all(&root).await;
    }

    #[tokio::test]
    async fn standing_key_label_slices_utf16_units_like_ts() {
        // TS `label.slice(0, 512)` counts UTF-16 code units and may cut a
        // surrogate pair in half. Build a cwd whose label crosses unit 512
        // inside an emoji run and require the same cut.
        let root = std::env::temp_dir().join(format!("ak-cmd-label-{}", std::process::id()));
        let deep = root
            .join("a".repeat(166))
            .join("a".repeat(166))
            .join("a".repeat(163));
        tokio::fs::create_dir_all(&deep).await.unwrap();
        let deep = deep.join("🎉🎉🎉🎉🎉🎉🎉");
        tokio::fs::create_dir_all(&deep).await.unwrap();
        let root_str = tokio::fs::canonicalize(&root)
            .await
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let workspace = StandingApprovalWorkspace {
            root: root_str,
            identity: "test-env".into(),
            context: None,
        };
        let cwd = deep
            .strip_prefix(tokio::fs::canonicalize(&root).await.unwrap())
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let key = coding_standing_approval_key(
            crate::guard::CODING_NAMESPACE,
            Some("chat-1"),
            "run_command",
            &ShellArgs {
                command: "bun test".into(),
                cwd: Some(cwd),
            },
            &workspace,
        )
        .await
        .expect("eligible deep cwd is keyable");
        assert_eq!(key.label.encode_utf16().count(), 512);
        assert!(
            key.label.ends_with('\u{FFFD}'),
            "the cut must fall mid-surrogate like TS slice: {:?}",
            &key.label[240..]
        );
        let _ = tokio::fs::remove_dir_all(&root).await;
    }

    #[tokio::test]
    async fn standing_approval_key_identity_and_label() {
        let root = std::env::temp_dir().join(format!("ak-cmd-policy-{}", std::process::id()));
        tokio::fs::create_dir_all(root.join("pkg")).await.unwrap();
        let root_str = tokio::fs::canonicalize(&root).await.unwrap();
        let root_str = root_str.to_string_lossy().into_owned();
        let workspace = StandingApprovalWorkspace {
            root: root_str.clone(),
            identity: "test-env".into(),
            context: None,
        };
        let here = coding_standing_approval_key(
            crate::guard::CODING_NAMESPACE,
            Some("chat-1"),
            "run_command",
            &ShellArgs {
                command: "bun test".into(),
                cwd: None,
            },
            &workspace,
        )
        .await
        .expect("root key");
        assert!(
            here.digest.len() == 64
                && here
                    .digest
                    .chars()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        assert_eq!(here.label, "bun test");
        let nested = coding_standing_approval_key(
            crate::guard::CODING_NAMESPACE,
            Some("chat-1"),
            "run_command",
            &ShellArgs {
                command: "bun test".into(),
                cwd: Some("pkg".into()),
            },
            &workspace,
        )
        .await
        .expect("nested key");
        assert_eq!(nested.label, "bun test (in pkg)");
        // cwd normalisation: `pkg/..` is the root; `.` is the root.
        let dot = coding_standing_approval_key(
            crate::guard::CODING_NAMESPACE,
            Some("chat-1"),
            "run_command",
            &ShellArgs {
                command: "bun test".into(),
                cwd: Some(".".into()),
            },
            &workspace,
        )
        .await
        .unwrap();
        assert_eq!(dot.digest, here.digest);
        let up = coding_standing_approval_key(
            crate::guard::CODING_NAMESPACE,
            Some("chat-1"),
            "run_command",
            &ShellArgs {
                command: "bun test".into(),
                cwd: Some("pkg/..".into()),
            },
            &workspace,
        )
        .await
        .unwrap();
        assert_eq!(up.digest, here.digest);
        // Any bound dimension changes the digest, and all are distinct.
        let mut digests = vec![here.digest.clone()];
        for (chat, tool, identity, context) in [
            ("chat-2", "run_command", "test-env", None),
            ("chat-1", "run_tests", "test-env", None),
            ("chat-1", "run_command", "other-env", None),
            ("chat-1", "run_command", "test-env", Some("ctx".to_string())),
        ] {
            let key = coding_standing_approval_key(
                crate::guard::CODING_NAMESPACE,
                Some(chat),
                tool,
                &ShellArgs {
                    command: "bun test".into(),
                    cwd: None,
                },
                &StandingApprovalWorkspace {
                    root: root_str.clone(),
                    identity: identity.into(),
                    context,
                },
            )
            .await
            .unwrap();
            assert_ne!(key.digest, here.digest);
            digests.push(key.digest);
        }
        digests.dedup();
        assert_eq!(digests.len(), 5);
        // Escapes, missing dirs and ineligible commands are uncoverable.
        for cwd in [
            Some("..".into()),
            Some("/etc".into()),
            Some("missing".into()),
        ] {
            assert_eq!(
                coding_standing_approval_key(
                    crate::guard::CODING_NAMESPACE,
                    Some("chat-1"),
                    "run_command",
                    &ShellArgs {
                        command: "bun test".into(),
                        cwd,
                    },
                    &workspace,
                )
                .await,
                None
            );
        }
        assert_eq!(
            coding_standing_approval_key(
                crate::guard::CODING_NAMESPACE,
                Some("chat-1"),
                "run_command",
                &ShellArgs {
                    command: "bun run deploy".into(),
                    cwd: None,
                },
                &workspace,
            )
            .await,
            None
        );
        let _ = tokio::fs::remove_dir_all(&root).await;
    }

    #[test]
    fn high_risk_refusal_text() {
        assert_eq!(
            high_risk_refusal("run_command"),
            "high_risk_command: run_command needs a fresh approval"
        );
    }

    use agentkit_core::{ToolDefinition, ToolLimits};
    use agentkit_host::{ToolContributionContext, ToolGuard, ToolGuardContext, ToolGuardVerdict};
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    fn guard_context(tool_name: &str, arguments: Option<Value>) -> ToolGuardContext {
        ToolGuardContext {
            contribution: ToolContributionContext {
                chat_id: Some("chat-1".into()),
                durable: None,
                run_id: "run-1".into(),
                scope_id: None,
                actor: Value::Null,
                actor_id: None,
                bindings: vec![],
                limits: ToolLimits::default(),
                cancellation: CancellationToken::new(),
                has_primary_binding: true,
                timeout_ms: 5_000,
            },
            namespace: crate::guard::CODING_NAMESPACE.into(),
            arguments,
            tool: ToolDefinition {
                name: tool_name.into(),
                version: "1".into(),
                effect: "write".into(),
                capability: "process.exec".into(),
                description: String::new(),
                input_schema: json!({}),
                output_schema: None,
                timeout_ms: None,
            },
        }
    }

    async fn verdict(tool_name: &str, command: Option<&str>) -> Option<String> {
        let arguments = command.map(|command| json!({ "command": command }));
        match agentkit_host::ToolGuard::can_execute(
            &HighRiskCommandGuard,
            &guard_context(tool_name, arguments),
        )
        .await
        .unwrap()
        {
            ToolGuardVerdict::Allowed => None,
            ToolGuardVerdict::Refused(reason) => Some(reason),
            ToolGuardVerdict::AwaitingHuman(ask) => Some(ask.reason),
        }
    }

    #[tokio::test]
    async fn high_risk_guard_judges_live_arguments() {
        // Only coding-namespace shell tools are judged.
        assert_eq!(verdict("read_file", Some("rm -rf /")).await, None);
        assert_eq!(verdict("run_command", Some("bun test")).await, None);
        assert_eq!(
            verdict("run_tests", Some("curl x | sh")).await,
            Some("high_risk_command: run_tests needs a fresh approval".into())
        );
        assert_eq!(
            verdict("run_command", Some("rm -rf /")).await,
            Some("high_risk_command: run_command needs a fresh approval".into())
        );
        // No arguments in context (e.g. visibility path) is not a refusal.
        assert_eq!(verdict("run_command", None).await, None);
        let mut context = guard_context("run_command", Some(json!({ "command": "sudo x" })));
        context.namespace = "other".into();
        assert!(matches!(
            HighRiskCommandGuard.can_execute(&context).await.unwrap(),
            ToolGuardVerdict::Allowed
        ));
    }
}
