import { executableName, shellWords } from "./command-words.js";

const HIGH_RISK_WORDS = new Set([
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

export function isHighRisk(raw: string): boolean {
  const words = [
    ...shellWords(raw),
    // Keep scanning substitutions inside double-quoted words without executing them.
    ...raw
      .replace(/["']/g, "")
      .split(/[\s;&|()<>`$]+/)
      .filter(Boolean),
  ];
  const tokens = words.map(executableName);
  if (hasInlineEvaluation(words)) return true;
  if (
    tokens.some((token) => /^(node(js)?|bun)$/.test(token)) &&
    raw.includes("<")
  )
    return true;
  if (
    tokens.includes("env") &&
    words.some(
      (word) => word.startsWith("--split-string") || word.startsWith("-S"),
    )
  )
    return true;
  // `find` is high risk only with an action flag, so it is judged per command.
  const hasFind = tokens.includes("find");
  for (const [index, token] of tokens.entries()) {
    if (HIGH_RISK_WORDS.has(token)) return true;
    // A flag names a build option (`--release`), not a target; only its value can.
    const word = token.startsWith("-") ? (token.split("=")[1] ?? "") : token;
    if (DANGEROUS_WORD.test(word)) return true;
    if (FORCE_FLAGS.has(token)) return true;
    if (
      token === "git" &&
      words
        .slice(index + 1)
        .some(
          (arg) =>
            isRiskyGitWord(executableName(arg)) ||
            arg === "-c" ||
            arg.startsWith("-c") ||
            arg.startsWith("--config-env"),
        )
    )
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

/** Inline code has arbitrary effects; it always needs a fresh human decision. */
function hasInlineEvaluation(words: string[]): boolean {
  return words.some((word, index) => {
    const token = executableName(word);
    const rest = words.slice(index + 1).map((arg) => arg.toLowerCase());
    if (/^(sh|bash|zsh|dash|ksh|fish)$/.test(token))
      return rest.some((arg) => /^-[a-z]*c/.test(arg));
    if (/^(python([0-9]+(\.[0-9]+)*)?|pypy[0-9]*)$/.test(token))
      return rest.some((arg) => arg === "-" || /^-[a-z]*c/.test(arg));
    if (
      /^node(js)?$/.test(token) &&
      (rest.length === 0 ||
        rest.every((arg) => arg.startsWith("-")) ||
        rest.some((arg) => arg === "-" || /^--input-type(=|$)/.test(arg)))
    )
      return true;
    if (
      /^(node(js)?|bun)$/.test(token) &&
      rest.some(
        (arg, i) =>
          /^--(import|loader|preload|require)=data:/.test(arg) ||
          (/^--(import|loader|preload|require)$/.test(arg) &&
            rest[i + 1]?.startsWith("data:")),
      )
    )
      return true;
    if (/^(node(js)?|bun|ruby|perl|lua|luajit)$/.test(token))
      return rest.some(
        (arg) => /^--(eval|print)(=|$)/.test(arg) || /^-[a-z]*[ep]/.test(arg),
      );
    if (token === "php") return rest.some((arg) => /^-r/.test(arg));
    if (token === "deno") return rest.includes("eval");
    if (/^(powershell|pwsh)(\.exe)?$/.test(token))
      return rest.some((arg) =>
        /^-(c|command|encodedcommand|enc|e)$/.test(arg),
      );
    return /^(awk|gawk|mawk)$/.test(token);
  });
}

function isRiskyGitWord(token: string): boolean {
  return GIT_RISKY_SUBCOMMANDS.has(token);
}

/** `-f` is forceful after git or rm (rm is already high risk by itself). */
function isForceHost(token: string): boolean {
  return token === "git" || token === "rm";
}
