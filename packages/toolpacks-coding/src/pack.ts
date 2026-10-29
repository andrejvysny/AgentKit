import type { AiTool } from "@agentkit/core";
import type { ToolSetContributor } from "@agentkit/host";
import { defaultWorkspaceCoordinator } from "./coordination.js";
import type { ExecutionCleanup } from "./coordination.js";
import { createEnv } from "./context.js";
import { gitInspectTool } from "./git-tool.js";
import { runCommandTool, runTestsTool } from "./exec-tools.js";
import {
  editFileTool,
  listDirTool,
  readFileTool,
  writeFileTool,
} from "./fs-tools.js";
import { CODING_NAMESPACE } from "./guard.js";
import { type CodingLimits, DEFAULT_CODING_LIMITS } from "./limits.js";
import { submitPlanTool } from "./plan-tool.js";
import { SANDBOX_EXEC } from "./sandbox.js";
import { codeSearchTool, findFilesTool, repoMapTool } from "./search-tools.js";
import type { CodingToolPackOptions } from "./types.js";
import { sweepOrphansDetailed } from "./process.js";

export const CODING_TOOL_NAMES: readonly string[] = Object.freeze([
  "read_file",
  "list_dir",
  "find_files",
  "code_search",
  "repo_map",
  "git_inspect",
  "edit_file",
  "write_file",
  "run_command",
  "run_tests",
  "submit_plan",
]);

function resolveLimits(
  partial: Partial<CodingLimits> | undefined,
): CodingLimits {
  const limits: CodingLimits = { ...DEFAULT_CODING_LIMITS, ...partial };
  for (const [name, value] of Object.entries(limits)) {
    if (typeof value !== "number" || !Number.isFinite(value) || value <= 0)
      throw new TypeError(`coding limit "${name}" must be a positive number`);
  }
  return limits;
}

/**
 * The coding tool pack as a host `ToolSetContributor` (namespace `coding`).
 * It offers every tool on every run; whether a call may proceed is the
 * `CodingPermissionGuard`'s decision, not the contributor's.
 */
export interface CodingToolPack extends ToolSetContributor {
  /** Close admission and await command teardown independently of turn promises. */
  drain(): Promise<ExecutionCleanup[]>;
}

let startupCleanup: Promise<ExecutionCleanup[]> | undefined;

function reconcileOrphans(): Promise<ExecutionCleanup[]> {
  startupCleanup ??= sweepOrphansDetailed().then((result) =>
    result.state === "stopped"
      ? []
      : [
          {
            executionAttemptId: `startup-orphans-${process.pid}`,
            state: result.state,
            survivors: result.survivors,
            reason: "orphan process reconciliation did not verify cleanup",
          },
        ],
  );
  return startupCleanup;
}

export function createCodingToolPack(
  options: CodingToolPackOptions,
): CodingToolPack {
  const root = options.workspace?.root;
  if (typeof root !== "string" || !root.startsWith("/"))
    throw new TypeError("workspace.root must be an absolute path");
  const limits = resolveLimits(options.limits);
  const coordinationWaitMs =
    options.coordinationWaitMs ??
    Math.min(20_000, Math.floor(limits.toolTimeoutMs * 0.66));
  if (
    !Number.isFinite(coordinationWaitMs) ||
    coordinationWaitMs <= 0 ||
    coordinationWaitMs >= limits.toolTimeoutMs
  )
    throw new TypeError(
      "coordinationWaitMs must be positive and below the toolTimeoutMs limit",
    );
  const env = createEnv({
    workspaceRoot: root,
    limits,
    allowSensitive: options.allowSensitive ?? false,
    clock: options.clock ?? Date.now,
    sandbox: options.sandbox ?? "required",
    network: options.network ?? false,
    allowGitWrites: options.allowGitWrites ?? false,
    testCommands: [...(options.testCommands ?? [])],
    platform: options.platform ?? process.platform,
    sandboxExecPath: options.sandboxExecPath ?? SANDBOX_EXEC,
    coordinator: options.coordinator ?? defaultWorkspaceCoordinator(),
    coordinationWaitMs,
    startupCleanup: reconcileOrphans(),
  });
  return {
    namespace: CODING_NAMESPACE,
    async contribute(): Promise<AiTool[]> {
      await env.root();
      return [
        readFileTool(env),
        listDirTool(env),
        findFilesTool(env),
        codeSearchTool(env),
        repoMapTool(env),
        gitInspectTool(env),
        editFileTool(env),
        writeFileTool(env),
        runCommandTool(env),
        runTestsTool(env),
        // Always offered: a session that enters plan mode needs the way out
        // present in the same run that enters it. Outside plan mode the guard
        // allows it and the tool reports, truthfully, what was granted.
        submitPlanTool(env, options.permission),
      ];
    },
    drain: () => env.drain(),
    async dispose() {
      await env.dispose();
    },
  };
}
