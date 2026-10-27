/**
 * Shell command classification and the coding pack's standing-approval policy.
 *
 * A standing approval lets a human say "yes, this exact command, for the rest of
 * this session". That is only safe for commands whose effect is bounded by what
 * they are, so eligibility is a POSITIVE allowlist over a character set with no
 * shell syntax in it: a command is `eligible` only when every character is
 * plain and its tokens spell one of a short list of build/test/inspect
 * invocations. Everything else is `unknown` and asks the human each time.
 * `high_risk` is a separate, earlier verdict used by the guard to force a fresh
 * approval even in `full_auto`.
 */
import { createHash } from "node:crypto";
import { realpath, stat } from "node:fs/promises";
import { relative } from "node:path";
import type {
  StandingApprovalKey,
  StandingApprovalPolicy,
  ToolGuard,
  ToolGuardVerdict,
} from "@agentkit/host";
import { CODING_NAMESPACE } from "./guard.js";
import { canonicalRoot, resolveInsideRoot } from "./paths.js";

export type CommandClass = "eligible" | "high_risk" | "unknown";

const SHELL_TOOLS = new Set(["run_command", "run_tests"]);

const HIGH_RISK_WORDS = new Set([
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
]);

const GIT_RISKY_SUBCOMMANDS = new Set([
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
]);

const PACKAGE_MANAGERS = new Set(["npm", "bun", "yarn", "pnpm", "cargo"]);
const PUBLISH_SUBCOMMANDS = new Set([
  "publish",
  "unpublish",
  "deprecate",
  "login",
  "logout",
  "adduser",
  "owner",
  "dist-tag",
]);
const FORCE_FLAGS = new Set(["--force", "--hard", "--no-verify"]);
const DANGEROUS_WORD =
  /(^|[:_-])(deploy|publish|release|migrate|migration|seed|drop|destroy|purge|wipe|prod|production)([:_-]|$)/i;

/** Whitespace plus every shell metacharacter that separates words or commands. */
const TOKEN_SPLIT = /[\s;&|()<>`$]+/;
/** The only characters an eligible command may contain. */
const PLAIN_CHARS = /^[A-Za-z0-9_./:=@%+, -]+$/;
const ASSIGNMENT = /^[A-Za-z_][A-Za-z0-9_]*=/;
const SAFE_SCRIPT =
  /^(test|lint|typecheck|type-check|check|build|format:check|fmt:check|(test|lint|typecheck|check):[A-Za-z0-9_.-]+)$/;

function isHighRisk(raw: string): boolean {
  const tokens = raw.split(TOKEN_SPLIT).filter((t) => t !== "");
  // `find` is high risk only with an action flag, so it is judged per command.
  const hasFind = tokens.includes("find");
  for (const [index, token] of tokens.entries()) {
    if (HIGH_RISK_WORDS.has(token)) return true;
    // A flag names a build option (`--release`), not a target; only its value can.
    const word = token.startsWith("-") ? (token.split("=")[1] ?? "") : token;
    if (DANGEROUS_WORD.test(word)) return true;
    if (FORCE_FLAGS.has(token)) return true;
    if (token === "git" && tokens.slice(index + 1).some(isRiskyGitWord))
      return true;
    if (
      PACKAGE_MANAGERS.has(token) &&
      tokens.slice(index + 1).some((t) => PUBLISH_SUBCOMMANDS.has(t))
    )
      return true;
    if (token === "-f" && tokens.slice(0, index).some(isForceHost)) return true;
  }
  return (
    hasFind &&
    tokens.some((t) => t === "-delete" || t === "-exec" || t === "-execdir")
  );
}

function isRiskyGitWord(token: string): boolean {
  return GIT_RISKY_SUBCOMMANDS.has(token);
}

/** `-f` is forceful after git or rm (rm is already high risk by itself). */
function isForceHost(token: string): boolean {
  return token === "git" || token === "rm";
}

function startsWith(tokens: string[], prefix: string[]): boolean {
  return prefix.every((word, i) => tokens[i] === word);
}

function matchesAllowlist(tokens: string[]): boolean {
  const [a, b, c] = tokens;
  if (startsWith(tokens, ["bun", "test"])) return true;
  if (
    (a === "bun" || a === "npm" || a === "pnpm" || a === "yarn") &&
    b === "run"
  )
    return tokens.length === 3 && c !== undefined && SAFE_SCRIPT.test(c);
  if ((a === "npm" || a === "pnpm" || a === "yarn") && b === "test")
    return tokens.length === 2;
  if (a === "tsc" || ((a === "npx" || a === "bunx") && b === "tsc"))
    return true;
  if (a === "vitest" && b === "run") return true;
  if (a === "npx" && b === "vitest" && c === "run") return true;
  if (a === "jest" || (a === "npx" && b === "jest")) return true;
  if (a === "eslint" || (a === "npx" && b === "eslint")) return true;
  if (a === "npx" && b === "prettier" && c === "--check") return true;
  if (a === "pytest") return true;
  if (
    (a === "python" || a === "python3") &&
    b === "-m" &&
    (c === "pytest" || c === "unittest")
  )
    return true;
  if (a === "uv" && b === "run" && c === "pytest") return true;
  if (
    a === "cargo" &&
    (b === "test" || b === "check" || b === "build" || b === "clippy")
  )
    return true;
  if (a === "cargo" && b === "fmt" && c === "--check") return true;
  if (a === "go" && (b === "test" || b === "build" || b === "vet")) return true;
  if (
    a === "make" &&
    tokens.length === 2 &&
    (b === "test" ||
      b === "check" ||
      b === "lint" ||
      b === "build" ||
      b === "typecheck")
  )
    return true;
  if (
    a === "git" &&
    (b === "status" || b === "diff" || b === "log" || b === "show")
  )
    return true;
  return false;
}

/**
 * `high_risk` is checked first and on the raw text, metacharacters included, so
 * `echo ok; rm -rf x` is high risk and not merely "unknown". `eligible` needs
 * the full allowlist; there is no "looks read-only" path to it.
 */
export function classifyShellCommand(command: string): CommandClass {
  if (isHighRisk(command)) return "high_risk";
  if (!PLAIN_CHARS.test(command)) return "unknown";
  const tokens = command.split(/ +/).filter((t) => t !== "");
  const first = tokens[0];
  if (first === undefined || ASSIGNMENT.test(first)) return "unknown";
  // A leading VAR=value changes the environment the command runs in.
  return matchesAllowlist(tokens) ? "eligible" : "unknown";
}

export interface CodingStandingApprovalOptions {
  /**
   * The workspace a chat's commands run in. `identity` names the environment the
   * approval is valid for (sandbox settings, host, user): change it and every
   * old grant stops matching. `context` is folded in verbatim for anything else
   * the host wants a grant bound to. Null: no workspace, no standing approval.
   */
  resolve(
    chatId: string,
  ): Promise<{ root: string; identity: string; context?: string } | null>;
}

interface ShellArgs {
  command: string;
  cwd?: string;
}

function shellArgsOf(args: unknown): ShellArgs | null {
  if (typeof args !== "object" || args === null || Array.isArray(args))
    return null;
  const { command, cwd } = args as { command?: unknown; cwd?: unknown };
  if (typeof command !== "string" || command === "") return null;
  if (cwd !== undefined && typeof cwd !== "string") return null;
  return { command, ...(cwd === undefined ? {} : { cwd }) };
}

/**
 * Standing approvals for the coding pack's `run_command` / `run_tests`.
 *
 * Covers only calls whose EVERY approvable refusal is the ordinary
 * `approval_required:` ("this mode asks for exec"); an uncertain-outcome, a
 * high-risk or a plan refusal makes the call uncoverable, because those say
 * "this particular call is special" and a remembered yes must not erase that.
 */
export function createCodingStandingApprovals(
  options: CodingStandingApprovalOptions,
): StandingApprovalPolicy {
  return {
    async keyFor(ctx, refusals): Promise<StandingApprovalKey | null> {
      if (ctx.namespace !== CODING_NAMESPACE) return null;
      if (!SHELL_TOOLS.has(ctx.tool.name)) return null;
      if (
        refusals.length === 0 ||
        !refusals.every((r) => r.reason.startsWith("approval_required:"))
      )
        return null;
      const args = shellArgsOf(ctx.arguments);
      if (args === null || ctx.chatId === undefined) return null;
      if (classifyShellCommand(args.command) !== "eligible") return null;
      const workspace = await options.resolve(ctx.chatId);
      if (workspace === null) return null;
      try {
        const root = await canonicalRoot(workspace.root);
        const confined = await resolveInsideRoot(root, args.cwd ?? ".");
        const cwd = await realpath(confined.path);
        if (!(await stat(cwd)).isDirectory()) return null;
        const inside = relative(root, cwd);
        if (inside === ".." || inside.startsWith("../")) return null;
        const digest = createHash("sha256")
          .update(
            JSON.stringify({
              v: 1,
              chatId: ctx.chatId,
              tool: ctx.tool.name,
              identity: workspace.identity,
              root,
              cwd,
              command: args.command,
              context: workspace.context ?? null,
            }),
          )
          .digest("hex");
        const label =
          inside === "" ? args.command : `${args.command} (in ${inside})`;
        return { digest, label: label.slice(0, 512) };
      } catch {
        return null;
      }
    },
  };
}

/**
 * Forces a FRESH approval for high-risk shell commands in every mode that would
 * otherwise run them. It adds an approvable refusal whose reason does not start
 * with `approval_required:`, which is what keeps the standing-approval policy
 * from ever covering the call. Role ceilings and plan mode still refuse final.
 */
export function createHighRiskCommandGuard(): ToolGuard {
  return {
    canExecute(ctx): ToolGuardVerdict {
      if (ctx.namespace !== CODING_NAMESPACE) return { allowed: true };
      const name = ctx.tool.name;
      if (!SHELL_TOOLS.has(name)) return { allowed: true };
      const args = shellArgsOf(ctx.arguments);
      if (args === null || classifyShellCommand(args.command) !== "high_risk")
        return { allowed: true };
      return {
        allowed: false,
        reason: `high_risk_command: ${name} needs a fresh approval`,
        approval: {},
      };
    },
  };
}
