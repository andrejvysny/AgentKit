import { execFile } from "node:child_process";
import type { AiTool } from "@agentkit/core";
import { asRecord, optBool, optInt, optString, reqString } from "./args.js";
import { type PackEnv, defineTool } from "./context.js";
import { CodingError } from "./errors.js";
import { SENSITIVE_GIT_EXCLUDES, confine } from "./paths.js";
import { formatSize } from "./truncate.js";

const COMMANDS = ["status", "diff", "log", "show"] as const;
type GitCommand = (typeof COMMANDS)[number];

const REV_PATTERN = /^[A-Za-z0-9_./~^-]{1,100}$/;

/**
 * Config that keeps git from running anything the repository supplies. Filter
 * and textconv drivers are named by the repository, so they are neutralized
 * per driver in `driverOverrides`; these are the ones with a fixed name.
 * `diff.external=` is inert only together with `--no-ext-diff`, which every
 * diff-capable command below passes.
 */
export const GIT_SAFETY_ARGS: readonly string[] = [
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

const DRIVER_KEY =
  /^(filter|diff)\.(.+)\.(clean|smudge|process|textconv|command)$/;
const DRIVER_KEY_REGEXP =
  "^(filter|diff)\\..*\\.(clean|smudge|process|textconv|command)$";

/**
 * Per-driver overrides for every filter/diff driver the repository config
 * defines. An empty `process` disables the long-running filter (git skips it
 * and falls back to clean/smudge, which are `cat`); an empty `textconv` or
 * `command` is likewise inert once --no-textconv/--no-ext-diff are passed.
 */
export function driverOverrides(keys: readonly string[]): string[] {
  const out: string[] = [];
  const seen = new Set<string>();
  for (const key of keys) {
    const m = DRIVER_KEY.exec(key);
    if (!m) continue;
    const [, section, name] = m;
    // -c splits on the first "=", so such a name cannot be overridden safely.
    if (!name || name.includes("=") || [...name].some((c) => c < " "))
      throw new CodingError(
        "git_config_unreadable",
        "a git driver name in the repository config cannot be neutralized",
      );
    const id = `${section}.${name}`;
    if (seen.has(id)) continue;
    seen.add(id);
    if (section === "filter")
      out.push(
        "-c",
        `filter.${name}.clean=cat`,
        "-c",
        `filter.${name}.smudge=cat`,
        "-c",
        `filter.${name}.process=`,
        "-c",
        `filter.${name}.required=false`,
      );
    else
      out.push("-c", `diff.${name}.textconv=`, "-c", `diff.${name}.command=`);
  }
  return out;
}

export interface GitInvocation {
  command: GitCommand;
  path?: string | undefined;
  staged?: boolean | undefined;
  maxCount?: number | undefined;
  rev?: string | undefined;
}

/** Fixed argv per command; user text only ever fills a value slot. */
export function buildGitArgs(
  root: string,
  call: GitInvocation,
  excludeSensitive: boolean,
  overrides: readonly string[] = [],
): string[] {
  const args = ["-C", root, ...GIT_SAFETY_ARGS, ...overrides];
  const pathspec = (): string[] => {
    const tail =
      call.path !== undefined || excludeSensitive
        ? ["--", call.path ?? "."]
        : [];
    return excludeSensitive && tail.length
      ? [...tail, ...SENSITIVE_GIT_EXCLUDES]
      : tail;
  };
  switch (call.command) {
    case "status":
      args.push("status", "--porcelain=v1", "--branch", ...pathspec());
      break;
    case "diff":
      args.push("diff", "--no-color", "--no-ext-diff", "--no-textconv");
      if (call.staged) args.push("--cached");
      if (call.rev) args.push(call.rev);
      args.push(...pathspec());
      break;
    case "log":
      args.push(
        "log",
        "--no-color",
        "--date=short",
        "--no-ext-diff",
        "--no-textconv",
        "--format=%h %ad %an %s",
        `--max-count=${call.maxCount ?? 20}`,
      );
      if (call.rev) args.push(call.rev);
      if (call.path !== undefined) args.push("--", call.path);
      break;
    case "show":
      args.push(
        "show",
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
        call.rev ?? "HEAD",
        ...pathspec(),
      );
      break;
  }
  return args;
}

interface GitRun {
  stdout: string;
  stderr: string;
  code: number | null;
  capped: boolean;
  timedOut: boolean;
}

/** Fixed env: no user config, no prompts, no pager, no external helpers. */
function gitEnv(): NodeJS.ProcessEnv {
  return {
    PATH: process.env["PATH"] ?? "",
    HOME: process.env["HOME"] ?? "",
    LANG: process.env["LANG"] ?? "C",
    GIT_TERMINAL_PROMPT: "0",
    GIT_OPTIONAL_LOCKS: "0",
    GIT_CONFIG_NOSYSTEM: "1",
    GIT_CONFIG_GLOBAL: "/dev/null",
    GIT_PAGER: "cat",
    GIT_EXTERNAL_DIFF: "",
    GIT_ASKPASS: "false",
    SSH_ASKPASS: "false",
  };
}

function runGit(
  args: string[],
  cap: number,
  timeoutMs: number,
  signal: AbortSignal | undefined,
): Promise<GitRun> {
  return new Promise((resolveRun) => {
    execFile(
      "git",
      args,
      {
        encoding: "utf8",
        maxBuffer: cap * 4,
        timeout: timeoutMs,
        ...(signal ? { signal } : {}),
        env: gitEnv(),
      },
      (error, stdout, stderr) => {
        const e = error as
          | (NodeJS.ErrnoException & { killed?: boolean; code?: unknown })
          | null;
        resolveRun({
          stdout: typeof stdout === "string" ? stdout : "",
          stderr: typeof stderr === "string" ? stderr : "",
          code: e ? (typeof e.code === "number" ? e.code : null) : 0,
          capped: e?.code === "ERR_CHILD_PROCESS_STDIO_MAXBUFFER",
          timedOut: e?.killed === true && e.code !== "ABORT_ERR",
        });
      },
    );
  });
}

/**
 * Names of every filter/diff driver the repository config (includes
 * honored) defines. Fails closed: if git cannot list them, nothing runs.
 */
async function configuredDrivers(
  root: string,
  timeoutMs: number,
  signal: AbortSignal | undefined,
): Promise<string[]> {
  const run = await runGit(
    [
      "-C",
      root,
      ...GIT_SAFETY_ARGS,
      "config",
      "-z",
      "--name-only",
      "--get-regexp",
      DRIVER_KEY_REGEXP,
    ],
    1 << 20,
    timeoutMs,
    signal,
  );
  // Exit 1 means "no matching key"; anything else is an unreadable config.
  if (run.timedOut || run.capped || (run.code !== 0 && run.code !== 1))
    throw new CodingError(
      "git_config_unreadable",
      `could not enumerate git drivers: ${run.stderr.trim().slice(0, 300) || `exit ${run.code}`}`,
    );
  return run.stdout.split("\0").filter((key) => key.length > 0);
}

export function gitInspectTool(env: PackEnv): AiTool {
  const l = env.limits;
  return defineTool(
    {
      name: "git_inspect",
      effect: "read",
      capability: "git.read",
      description: `Read-only git inspection: status, diff (staged or not), log, show. Fixed commands only. Hooks, fsmonitor, filter/textconv/external-diff drivers, pager and ssh/askpass helpers configured by the repository are neutralized and never run; if the repository config cannot be read the call is refused. Output is capped at ${formatSize(l.gitMaxOutputBytes)}. Prefer this over running git through a shell.`,
      inputSchema: {
        type: "object",
        properties: {
          command: { type: "string", enum: [...COMMANDS] },
          args: {
            type: "object",
            properties: {
              path: {
                type: "string",
                description:
                  "Limit to this path (relative to the workspace root).",
              },
              staged: {
                type: "boolean",
                description: "diff only: show staged changes.",
              },
              maxCount: {
                type: "integer",
                minimum: 1,
                maximum: l.gitMaxCount,
                description: "log only.",
              },
              rev: {
                type: "string",
                maxLength: 100,
                description: "Commit, branch, tag or range, e.g. HEAD~2.",
              },
            },
            additionalProperties: false,
          },
        },
        required: ["command"],
        additionalProperties: false,
      },
      timeoutMs: l.gitTimeoutMs + 5000,
    },
    async (ctx, raw) => {
      const input = asRecord(raw);
      const command = reqString(input, "command", 16);
      if (!(COMMANDS as readonly string[]).includes(command))
        throw new CodingError(
          "git_command_not_allowed",
          `git command "${command.slice(0, 40)}" is not allowed; use one of ${COMMANDS.join(", ")}`,
        );
      const args = input["args"] === undefined ? {} : asRecord(input["args"]);
      const rev = optString(args, "rev", 100);
      if (rev !== undefined && (rev.startsWith("-") || !REV_PATTERN.test(rev)))
        throw new CodingError(
          "invalid_rev",
          "rev must match [A-Za-z0-9_./~^-]{1,100} and not start with -",
        );
      const root = await env.root();
      const rawPath = optString(args, "path");
      const target =
        rawPath === undefined
          ? undefined
          : await confine(root, rawPath, {
              access: "read",
              allowSensitive: env.allowSensitive,
            });
      // A path inside .git is metadata, not source: only the fixed commands read it.
      const call: GitInvocation = {
        command: command as GitCommand,
        path: target ? target.relativePath || "." : undefined,
        staged: optBool(args, "staged"),
        maxCount: optInt(args, "maxCount", 1, l.gitMaxCount),
        rev,
      };
      const drivers = await configuredDrivers(root, l.gitTimeoutMs, ctx.signal);
      if (ctx.signal?.aborted)
        throw new CodingError("aborted", "the call was aborted");
      const run = await runGit(
        buildGitArgs(root, call, !env.allowSensitive, driverOverrides(drivers)),
        l.gitMaxOutputBytes,
        l.gitTimeoutMs,
        ctx.signal,
      );
      if (ctx.signal?.aborted)
        throw new CodingError("aborted", "the call was aborted");
      if (run.timedOut)
        throw new CodingError("git_timeout", `git ${command} timed out`);
      if (!run.capped && run.code !== 0)
        return {
          ok: false,
          text: `git_failed: ${(run.stderr || run.stdout).trim().slice(0, 2000) || `exit ${run.code}`}`,
          summary: `git ${command} failed`,
        };
      const bytes = Buffer.from(run.stdout, "utf-8");
      const over = run.capped || bytes.length > l.gitMaxOutputBytes;
      const text = over
        ? `${bytes.subarray(0, l.gitMaxOutputBytes).toString("utf-8")}\n[output truncated at ${formatSize(l.gitMaxOutputBytes)}; narrow with args.path or args.rev]`
        : run.stdout;
      return {
        ok: true,
        text: text.trim().length ? text : "(no output)",
        summary: `git ${command}`,
        truncated: over,
      };
    },
  );
}
