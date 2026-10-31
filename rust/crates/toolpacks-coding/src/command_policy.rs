//! Shell command classification and the coding pack's standing-approval
//! policy. Port of `packages/toolpacks-coding/src/command-policy.ts`.
//!
//! A standing approval lets a human say "yes, this exact command, for the
//! rest of this session". That is only safe for commands whose effect is
//! bounded by what they are, so eligibility is a POSITIVE allowlist over a
//! character set with no shell syntax in it: a command is `eligible` only
//! when every character is plain and its tokens spell one of a short list
//! of build/test/inspect invocations. Other commands are `unknown` and follow
//! the configured mode. `high_risk` is a separate, earlier verdict used by
//! the guard to force a fresh approval even in `full_auto`.
//!
//! Native standing approvals are disabled until executable, script,
//! configuration, and environment identity can be verified on each call.
//! [`HighRiskCommandGuard`] forces a fresh approval for high-risk commands.

use serde_json::Value;

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
        "rimraf",
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

fn shell_words(raw: &str) -> Vec<String> {
    let mut words = vec![];
    let mut word = String::new();
    let mut quote = None;
    let mut chars = raw.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' && quote != Some('\'') && chars.peek().is_some() {
            word.push(chars.next().expect("peeked"));
        } else if quote == Some(ch) {
            quote = None;
        } else if quote.is_none() && (ch == '\'' || ch == '"') {
            quote = Some(ch);
        } else if quote.is_none() && (ch.is_whitespace() || ";&|()<>`$".contains(ch)) {
            if !word.is_empty() {
                words.push(std::mem::take(&mut word));
            }
        } else {
            word.push(ch);
        }
    }
    if !word.is_empty() {
        words.push(word);
    }
    words
}

fn executable_name(word: &str) -> String {
    word.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .to_lowercase()
}

fn has_inline_evaluation(words: &[String]) -> bool {
    words.iter().enumerate().any(|(index, word)| {
        let token = executable_name(word);
        let rest: Vec<String> = words[index + 1..]
            .iter()
            .map(|arg| arg.to_lowercase())
            .collect();
        inline_shell(&token, &rest) || inline_node(&token, &rest) || inline_other(&token, &rest)
    })
}

fn inline_shell(token: &str, rest: &[String]) -> bool {
    static SHELL_CODE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| compiled(r"^-[a-z]*c"));
    static PYTHON: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| compiled(r"^(python([0-9]+(\.[0-9]+)*)?|pypy[0-9]*)$"));
    if matches!(token, "sh" | "bash" | "zsh" | "dash" | "ksh" | "fish") {
        return rest.iter().any(|arg| SHELL_CODE.is_match(arg));
    }
    PYTHON.is_match(token)
        && rest
            .iter()
            .any(|arg| arg == "-" || SHELL_CODE.is_match(arg))
}

fn inline_node(token: &str, rest: &[String]) -> bool {
    if matches!(token, "node" | "nodejs")
        && (rest.is_empty()
            || rest.iter().all(|arg| arg.starts_with('-'))
            || rest
                .iter()
                .any(|arg| arg == "-" || arg == "--input-type" || arg.starts_with("--input-type=")))
    {
        return true;
    }
    matches!(token, "node" | "nodejs" | "bun")
        && rest.iter().enumerate().any(|(i, arg)| {
            ["--import", "--loader", "--preload", "--require"]
                .iter()
                .any(|flag| {
                    arg.starts_with(&format!("{flag}=data:"))
                        || (arg == flag
                            && rest
                                .get(i + 1)
                                .is_some_and(|next| next.starts_with("data:")))
                })
        })
}

fn inline_other(token: &str, rest: &[String]) -> bool {
    static EVAL_SHORT: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| compiled(r"^-[a-z]*[ep]"));
    if matches!(
        token,
        "node" | "nodejs" | "bun" | "ruby" | "perl" | "lua" | "luajit"
    ) {
        return rest.iter().any(|arg| {
            arg == "--eval"
                || arg.starts_with("--eval=")
                || arg == "--print"
                || arg.starts_with("--print=")
                || EVAL_SHORT.is_match(arg)
        });
    }
    if token == "php" {
        return rest.iter().any(|arg| arg.starts_with("-r"));
    }
    if token == "deno" {
        return rest.iter().any(|arg| arg == "eval");
    }
    if matches!(token, "powershell" | "powershell.exe" | "pwsh" | "pwsh.exe") {
        return rest.iter().any(|arg| {
            matches!(
                arg.as_str(),
                "-c" | "-command" | "-encodedcommand" | "-enc" | "-e"
            )
        });
    }
    matches!(token, "awk" | "gawk" | "mawk")
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
    let stripped = raw.replace(['\'', '"'], "");
    let mut words = shell_words(raw);
    words.extend(
        TOKEN_SPLIT
            .split(&stripped)
            .filter(|word| !word.is_empty())
            .map(str::to_owned),
    );
    let tokens: Vec<String> = words.iter().map(|word| executable_name(word)).collect();
    if has_inline_evaluation(&words) {
        return true;
    }
    if tokens
        .iter()
        .any(|t| matches!(t.as_str(), "node" | "nodejs" | "bun"))
        && raw.contains('<')
    {
        return true;
    }
    if tokens.iter().any(|t| t == "env")
        && words
            .iter()
            .any(|word| word.starts_with("--split-string") || word.starts_with("-S"))
    {
        return true;
    }
    // `find` is high risk only with an action flag, so it is judged per
    // command.
    let has_find = tokens.iter().any(|token| token == "find");
    for (index, token) in tokens.iter().enumerate() {
        if high_risk_words().contains(&token.as_str()) {
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
        if force_flags().contains(&token.as_str()) {
            return true;
        }
        if token == "git"
            && (tokens[index + 1..].iter().any(|t| is_risky_git_word(t))
                || words[index + 1..]
                    .iter()
                    .any(|arg| arg.starts_with("-c") || arg.starts_with("--config-env")))
        {
            return true;
        }
        if package_managers().contains(&token.as_str())
            && tokens[index + 1..]
                .iter()
                .any(|t| publish_subcommands().contains(&t.as_str()))
        {
            return true;
        }
        if token == "-f" && tokens[..index].iter().any(|t| is_force_host(t)) {
            return true;
        }
    }
    has_find
        && tokens
            .iter()
            .any(|t| t == "-delete" || t == "-exec" || t == "-execdir")
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

/// Decline standing approval until native execution inputs can be fingerprinted.
pub async fn coding_standing_approval_key(
    namespace: &str,
    chat_id: Option<&str>,
    tool: &str,
    args: &ShellArgs,
    workspace: &StandingApprovalWorkspace,
) -> Option<StandingApprovalKey> {
    // Without executable, script, configuration, and environment identity,
    // any remembered approval can outlive a changed command implementation.
    let _ = (namespace, chat_id, tool, args, workspace);
    None
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
    fn indirect_risk_requires_a_fresh_approval() {
        for command in [
            "npx rimraf x",
            "git -c core.sshCommand=evil status",
            "git -ccore.sshCommand=evil status",
            "/usr/bin/git --config-env=core.sshCommand=EVIL status",
            "\"rimraf\" x",
            "r\\m x",
            "/bin/rm x",
            "node -",
            "bash -c 'printf inert'",
            "node --input-type=module -",
            "node < ./input.js",
            "node --import=data:text/javascript,console.log(1)",
            "env -S '/bin/rm inert'",
            "python3 -c 'print(1)'",
            "echo \"$(/bin/rm inert)\"",
            "'rm' inert",
            "r''m inert",
            "env SAFE=1 /bin/rm inert",
            "env --split-string=\"rm inert\"",
            "python3 -Ic'print(1)'",
            "node --eval='console.log(1)'",
            "node -p '1'",
            "bun -e '1'",
            "deno eval '1'",
            "perl -e 'print 1'",
            "ruby -e 'puts 1'",
            "php -r 'echo 1;'",
            "pwsh -EncodedCommand inert",
            "awk '{print 1}'",
            "node --input-type module",
            "printf inert | node",
            "node --import 'data:text/javascript,console.log(1)'",
            "node --loader=data:text/javascript,console.log(1) script.js",
            "bun --preload=data:text/javascript,console.log(1) test",
        ] {
            assert_eq!(
                classify_shell_command(command),
                CommandClass::HighRisk,
                "{command}"
            );
        }
        assert_eq!(
            classify_shell_command("git -C /repo status"),
            CommandClass::Unknown
        );
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
        assert_eq!(
            coding_standing_approval_key(
                crate::guard::CODING_NAMESPACE,
                Some("chat-1"),
                "run_command",
                &args,
                &workspace
            )
            .await,
            None,
            "unfingerprinted calls must re-ask"
        );
        let _ = tokio::fs::remove_dir_all(&root).await;
    }

    #[tokio::test]
    async fn legacy_unbound_digest_is_never_issued() {
        // The former v1 digest omitted executable and environment identity.
        // It cannot authorize any standing grant, even for an eligible command.
        let workspace = StandingApprovalWorkspace {
            root: "/".into(),
            identity: "test".into(),
            context: None,
        };
        for command in ["bun test", "npm run test", "npx tsc --noEmit", "vitest run"] {
            let args = ShellArgs {
                command: command.into(),
                cwd: None,
            };
            assert_eq!(
                coding_standing_approval_key(
                    crate::guard::CODING_NAMESPACE,
                    Some("chat-1"),
                    "run_command",
                    &args,
                    &workspace
                )
                .await,
                None,
                "{command}"
            );
        }
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
