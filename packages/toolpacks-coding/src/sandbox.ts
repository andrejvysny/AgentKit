/**
 * macOS Seatbelt (`sandbox-exec`) profile for the shell tools.
 *
 * Adapted from OneMind (https://github.com/andrejvysny/OneMind), commit
 * 100dad7, path `src-ts/src/code/pi-worker/shell-isolation.ts` (lines 5-90:
 * `SAFE_PATH`, denied read roots, credential patterns, toolchain dirs).
 * Copyright (c) 2026 OneMind contributors. MIT License.
 * Changes: the workspace root is writable (OneMind's shell is read-only),
 * network is opt-in, `.git` is read-only (hooks and config only, with
 * allowGitWrites) so a command cannot plant a hook or driver that later runs
 * unsandboxed.
 */
import { existsSync, realpathSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, isAbsolute, join } from "node:path";
import { CodingError } from "./errors.js";

export const SANDBOX_EXEC = "/usr/bin/sandbox-exec";
const SAFE_PATH = /^\/[A-Za-z0-9._/-]{1,512}$/;

// Home, volumes and shared temp dirs hold credentials and other projects.
const DENIED_READ_ROOTS = [
  "/Users",
  "/Volumes",
  "/private/tmp",
  "/private/var/folders",
  "/private/var/root",
];
const CREDENTIAL_PATTERNS = [
  "/\\.(netrc|npmrc|pypirc|git-credentials)$",
  "/\\.cargo/credentials(\\.toml)?$",
  "/\\.(ssh|aws|gnupg|kube|docker|azure|terraform\\.d|password-store)(/|$)",
  "/\\.pgpass$",
  "/\\.config/(gh|gcloud|hub)(/|$)",
  "/\\.local/share/keyrings(/|$)",
  "/Library/(Keychains|Cookies)(/|$)",
];
const TOOLCHAIN_DIRS = [
  ".rustup",
  ".cargo",
  ".bun",
  ".nvm",
  ".volta",
  ".deno",
  ".pyenv",
  ".rbenv",
  ".pixi",
  ".local/share/mise",
  "go/pkg/mod",
];

export interface SandboxPolicy {
  root: string;
  scratch: string;
  network: boolean;
  /** False: the whole `.git` subtree is read-only; true: only hooks and config are. */
  allowGitWrites: boolean;
  /** Extra absolute canonical directories the command may read. */
  readable: readonly string[];
}

export interface SandboxEnvironment {
  platform: NodeJS.Platform;
  sandboxExecPath: string;
}

/** True only where the OS can really enforce the profile. */
export function sandboxAvailable(env: SandboxEnvironment): boolean {
  return env.platform === "darwin" && existsSync(env.sandboxExecPath);
}

/** SBPL applies the last matching rule: broad deny, narrow allow, final deny. */
export function sandboxProfile(policy: SandboxPolicy): string {
  const paths = [policy.scratch, policy.root, ...policy.readable];
  if (!paths.every((path) => SAFE_PATH.test(path)))
    throw new CodingError(
      "shell_isolation_unavailable",
      "a path needed by the sandbox profile contains characters outside [A-Za-z0-9._/-]; refusing to build the profile",
    );
  const subpath = (path: string) => `(subpath "${path}")`;
  // Config, hooks, attributes, refs and objects all steer later unsandboxed
  // git runs, so the default is to fence the whole subtree.
  const gitGuards = policy.allowGitWrites
    ? [join(policy.root, ".git", "hooks"), join(policy.root, ".git", "config")]
    : [join(policy.root, ".git")];
  return (
    "(version 1)(allow default)" +
    (policy.network ? "" : "(deny network*)") +
    "(deny file-write*)" +
    `(allow file-write* ${subpath(policy.root)} ${subpath(policy.scratch)} (literal "/dev/null") (literal "/dev/tty") (literal "/dev/dtracehelper") (regex #"^/dev/fd/[0-9]+$"))` +
    `(deny file-write* ${gitGuards.map(subpath).join(" ")})` +
    `(deny file-read-data ${DENIED_READ_ROOTS.map(subpath).join(" ")})` +
    `(allow file-read-data ${paths.map(subpath).join(" ")})` +
    `(deny file-read-data ${CREDENTIAL_PATTERNS.map((pattern) => `(regex #"${pattern}")`).join(" ")})`
  );
}

function realDirectory(path: string): string | undefined {
  try {
    return realpathSync(path);
  } catch {
    return undefined;
  }
}

/** Toolchains under $HOME plus the running node/bun executable's directory. */
export function toolchainPaths(env: NodeJS.ProcessEnv): string[] {
  const home = env.HOME || homedir();
  const found: string[] = [];
  for (const entry of (env.PATH ?? "").split(":")) {
    if (
      !entry ||
      !isAbsolute(entry) ||
      !(entry === home || entry.startsWith(`${home}/`))
    )
      continue;
    // Only the bin dir itself: a tool's home may hold its auth tokens.
    const real = realDirectory(entry);
    if (real) found.push(real);
  }
  for (const dir of TOOLCHAIN_DIRS) {
    const real = realDirectory(join(home, dir));
    if (real) found.push(real);
  }
  const exec = realDirectory(dirname(process.execPath));
  if (exec) found.push(exec);
  return [...new Set(found)].filter((path) => SAFE_PATH.test(path)).sort();
}
