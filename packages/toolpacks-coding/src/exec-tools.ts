import { randomBytes } from "node:crypto";
import { stat } from "node:fs/promises";
import type { AiTool, AiToolExecutionContext } from "@agentkit/core";
import { asRecord, optInt, optString, reqString } from "./args.js";
import { type Outcome, type PackEnv, defineTool } from "./context.js";
import { CodingError } from "./errors.js";
import { confine } from "./paths.js";
import { runBounded } from "./process.js";
import { sandboxAvailable, sandboxProfile, toolchainPaths } from "./sandbox.js";

// `AUTH(?!OR)`: GIT_AUTHOR_NAME/EMAIL are commit identity, not credentials.
const SECRET_NAME =
  /(TOKEN|SECRET|PASSWORD|PASSWD|KEY|CREDENTIAL|AUTH(?!OR)|COOKIE|SESSION|PRIVATE)/i;
const AGENT_VARS = new Set(["SSH_AUTH_SOCK", "GPG_AGENT_INFO"]);
/** A URL with userinfo (`scheme://user:pass@host`) carries a credential. */
const CREDENTIAL_URL = /^[a-z][a-z0-9+.-]*:\/\/[^/\s:@]+:[^/\s@]+@/i;
const MAX_COMMAND_CHARS = 16_384;

/** Process env minus secrets; PATH, HOME, LANG and TERM always survive. */
export function sanitizedEnv(
  source: NodeJS.ProcessEnv,
  scratch: string,
): NodeJS.ProcessEnv {
  const env: NodeJS.ProcessEnv = {};
  for (const [name, value] of Object.entries(source)) {
    if (
      value === undefined ||
      AGENT_VARS.has(name) ||
      SECRET_NAME.test(name) ||
      CREDENTIAL_URL.test(value)
    )
      continue;
    env[name] = value;
  }
  env["PATH"] = source["PATH"] ?? "/usr/bin:/bin:/usr/sbin:/sbin";
  env["HOME"] = source["HOME"] ?? scratch;
  env["LANG"] = source["LANG"] ?? "C.UTF-8";
  env["TERM"] = source["TERM"] ?? "dumb";
  env["TMPDIR"] = scratch;
  return env;
}

const SURVIVOR_RECHECK_MS = 2000;

/** Pids from `pids` still alive after waiting up to `ms` for them to exit. */
async function waitForExit(pids: number[], ms: number): Promise<number[]> {
  let alive = pids;
  const deadline = Date.now() + ms;
  while (alive.length > 0) {
    alive = alive.filter((pid) => {
      try {
        process.kill(pid, 0);
        return true;
      } catch {
        return false; // ESRCH (gone) or EPERM (not ours): not ours to wait on
      }
    });
    if (alive.length === 0 || Date.now() >= deadline) break;
    await new Promise((done) => setTimeout(done, 50));
  }
  return alive;
}

interface ShellRequest {
  tool: string;
  command: string;
  cwd: string | undefined;
  timeoutSec: number;
}

async function runShell(
  env: PackEnv,
  ctx: AiToolExecutionContext,
  request: ShellRequest,
): Promise<Outcome> {
  const sandboxed = env.sandbox === "required";
  if (
    sandboxed &&
    !sandboxAvailable({
      platform: env.platform,
      sandboxExecPath: env.sandboxExecPath,
    })
  )
    throw new CodingError(
      "shell_isolation_unavailable",
      `sandbox-exec is not available on this host (${env.platform}); refusing to run an unsandboxed command. Set sandbox: "off" explicitly to accept that.`,
    );
  const root = await env.root();
  const cwd = await confine(root, request.cwd ?? ".", {
    access: "read",
    allowSensitive: env.allowSensitive,
  });
  const info = await stat(cwd.path).catch(() => null);
  if (!info?.isDirectory())
    throw new CodingError("not_a_directory", "cwd is not a directory");
  const scratch = await env.scratch();
  const base = process.env;
  const file = sandboxed ? env.sandboxExecPath : "/bin/bash";
  const args = sandboxed
    ? [
        "-p",
        sandboxProfile({
          root,
          scratch,
          network: env.network,
          allowGitWrites: env.allowGitWrites,
          readable: toolchainPaths(base),
        }),
        "/bin/bash",
        "-c",
        request.command,
      ]
    : ["-c", request.command];
  // Every command may write, so every command takes the exclusive lease; it is
  // held from before spawn until the process tree has been torn down (and, if
  // survivors remain, until they are gone or a short re-check gives up).
  const lease = await env.coordinator.acquireWorkspace({
    root,
    holder: {
      ...(ctx.chatId !== undefined ? { chatId: ctx.chatId } : {}),
      runId: ctx.runId,
      ...(ctx.toolCallId !== undefined ? { toolCallId: ctx.toolCallId } : {}),
      tool: request.tool,
      label: request.command.slice(0, 200),
    },
    signal: ctx.signal,
    maxWaitMs: env.coordinationWaitMs,
  });
  let stragglers: number[] = [];
  let run: Awaited<ReturnType<typeof runBounded>>;
  try {
    run = await runBounded({
      file,
      args,
      cwd: cwd.path,
      env: sanitizedEnv(base, scratch),
      timeoutMs: request.timeoutSec * 1000,
      signal: ctx.signal,
      scratch,
      logName: `command-${env.clock()}-${randomBytes(4).toString("hex")}.log`,
      limits: env.limits,
    });
    stragglers = await waitForExit(run.teardown.survivors, SURVIVOR_RECHECK_MS);
  } finally {
    await lease.release();
  }
  if (run.spawnError)
    throw new CodingError("spawn_failed", run.spawnError.slice(0, 500));
  const footer = run.timedOut
    ? `[timed out after ${request.timeoutSec}s; process group killed${run.signal ? ` (${run.signal})` : ""}]`
    : run.aborted
      ? "[aborted; process group killed]"
      : run.signal
        ? `[killed by ${run.signal}]`
        : `[exit code ${run.exitCode}]`;
  const parts = [run.output.replace(/\n+$/, ""), run.notice, footer].filter(
    (p) => p.length > 0,
  );
  const ok = run.exitCode === 0 && !run.timedOut && !run.aborted;
  return {
    ok,
    text: parts.join("\n"),
    summary: run.timedOut
      ? `timed out after ${request.timeoutSec}s`
      : run.aborted
        ? "aborted"
        : run.signal
          ? `killed by ${run.signal}`
          : `exit code ${run.exitCode}`,
    truncated: run.truncated,
    warnings: [
      ...(sandboxed ? [] : ["sandbox is off: the command ran unconfined"]),
      ...(run.teardown.survivors.length > 0
        ? [`process_survivors: ${run.teardown.survivors.join(",")}`]
        : []),
      ...(stragglers.length > 0
        ? [
            `lease released while processes were still running: ${stragglers.join(",")}`,
          ]
        : []),
    ],
    details: {
      exitCode: run.exitCode,
      signal: run.signal,
      timedOut: run.timedOut,
      teardown: run.teardown,
      ...(run.logPath ? { fullLog: run.logPath } : {}),
    },
  };
}

export function runCommandTool(env: PackEnv): AiTool {
  const l = env.limits;
  return defineTool(
    {
      name: "run_command",
      effect: "write",
      capability: "process.exec",
      description: `Run a shell command (bash -c) in the workspace${env.sandbox === "required" ? ", inside a macOS sandbox: writes only in the workspace, no network" + (env.network ? " restrictions lifted" : "") + ", no access to credentials" : " WITHOUT a sandbox"}. Use it for builds, tests and tooling. Do NOT use it to read or search files, list directories or inspect git: use read_file, list_dir, find_files, code_search, repo_map and git_inspect, which are cheaper and safer. Output keeps the last ${l.commandTailMaxLines} lines / ${l.commandTailMaxBytes / 1024} KB. Default timeout ${l.commandDefaultTimeoutSec}s, max ${l.commandMaxTimeoutSec}s.`,
      inputSchema: {
        type: "object",
        properties: {
          command: { type: "string", description: "Shell command line." },
          cwd: {
            type: "string",
            description: "Directory inside the workspace. Default: the root.",
          },
          timeoutSec: {
            type: "integer",
            minimum: 1,
            maximum: l.commandMaxTimeoutSec,
          },
        },
        required: ["command"],
        additionalProperties: false,
      },
      timeoutMs: (l.commandMaxTimeoutSec + 30) * 1000,
    },
    async (ctx, raw) => {
      const input = asRecord(raw);
      return runShell(env, ctx, {
        tool: "run_command",
        command: reqString(input, "command", MAX_COMMAND_CHARS),
        cwd: optString(input, "cwd"),
        timeoutSec:
          optInt(input, "timeoutSec", 1, l.commandMaxTimeoutSec) ??
          l.commandDefaultTimeoutSec,
      });
    },
  );
}

export function runTestsTool(env: PackEnv): AiTool {
  const l = env.limits;
  const allowed = env.testCommands.length
    ? env.testCommands.map((c) => `"${c}"`).join(", ")
    : "(none configured)";
  return defineTool(
    {
      name: "run_tests",
      effect: "write",
      capability: "process.exec",
      description: `Run one of the project's configured test commands, exactly as written: ${allowed}. Same sandbox and output limits as run_command.`,
      inputSchema: {
        type: "object",
        properties: {
          command: {
            type: "string",
            description: "One of the allowed test commands, verbatim.",
          },
        },
        required: ["command"],
        additionalProperties: false,
      },
      timeoutMs: (l.commandMaxTimeoutSec + 30) * 1000,
    },
    async (ctx, raw) => {
      const command = reqString(asRecord(raw), "command", MAX_COMMAND_CHARS);
      if (!env.testCommands.includes(command))
        throw new CodingError(
          "test_command_not_allowed",
          `"${command.slice(0, 200)}" is not a configured test command. Allowed: ${allowed}`,
        );
      return runShell(env, ctx, {
        tool: "run_tests",
        command,
        cwd: undefined,
        timeoutSec: l.commandMaxTimeoutSec,
      });
    },
  );
}
