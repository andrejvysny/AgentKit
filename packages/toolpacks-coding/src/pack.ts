import type { AiTool } from "@agentkit/core";
import type { ToolSetContributor } from "@agentkit/host";
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
export function createCodingToolPack(
  options: CodingToolPackOptions,
): ToolSetContributor {
  const root = options.workspace?.root;
  if (typeof root !== "string" || !root.startsWith("/"))
    throw new TypeError("workspace.root must be an absolute path");
  const env = createEnv({
    workspaceRoot: root,
    limits: resolveLimits(options.limits),
    allowSensitive: options.allowSensitive ?? false,
    clock: options.clock ?? Date.now,
    sandbox: options.sandbox ?? "required",
    network: options.network ?? false,
    allowGitWrites: options.allowGitWrites ?? false,
    testCommands: [...(options.testCommands ?? [])],
    platform: options.platform ?? process.platform,
    sandboxExecPath: options.sandboxExecPath ?? SANDBOX_EXEC,
  });
  return {
    namespace: CODING_NAMESPACE,
    async contribute(): Promise<AiTool[]> {
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
    async dispose() {
      await env.dispose();
    },
  };
}
