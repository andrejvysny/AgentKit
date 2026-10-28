import { mkdtemp, rm } from "node:fs/promises";
import { realpathSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type {
  AiJsonSchemaObject,
  AiToolDefinition,
  AiToolResult,
} from "@agentkit/contracts";
import type { AiTool, AiToolExecutionContext } from "@agentkit/core";
import type { ExecutionCleanup, WorkspaceCoordinator } from "./coordination.js";
import { CodingCleanupError, CodingError } from "./errors.js";
import type { CodingLimits } from "./limits.js";
import { canonicalRoot } from "./paths.js";
import { CommandLifecycle } from "./process-lifecycle.js";

/** What every tool body needs; built once per pack. */
export interface PackEnv {
  limits: CodingLimits;
  allowSensitive: boolean;
  clock: () => number;
  sandbox: "required" | "off";
  network: boolean;
  allowGitWrites: boolean;
  testCommands: readonly string[];
  platform: NodeJS.Platform;
  sandboxExecPath: string;
  coordinator: WorkspaceCoordinator;
  /** Longest a mutation waits for a lease before `workspace_busy`. */
  coordinationWaitMs: number;
  /** Canonical workspace root (resolved lazily, cached). */
  root(): Promise<string>;
  /** Private scratch directory outside the workspace (created lazily). */
  scratch(): Promise<string>;
  commands: CommandLifecycle;
  drain(): Promise<ExecutionCleanup[]>;
  dispose(): Promise<void>;
}

export interface EnvInit {
  workspaceRoot: string;
  limits: CodingLimits;
  allowSensitive: boolean;
  clock: () => number;
  sandbox: "required" | "off";
  network: boolean;
  allowGitWrites: boolean;
  testCommands: readonly string[];
  platform: NodeJS.Platform;
  sandboxExecPath: string;
  coordinator: WorkspaceCoordinator;
  coordinationWaitMs: number;
  startupCleanup?: Promise<ExecutionCleanup[]>;
}

export function createEnv(init: EnvInit): PackEnv {
  let root: Promise<string> | undefined;
  let scratch: Promise<string> | undefined;
  const commands = new CommandLifecycle();
  return {
    limits: init.limits,
    allowSensitive: init.allowSensitive,
    clock: init.clock,
    sandbox: init.sandbox,
    network: init.network,
    allowGitWrites: init.allowGitWrites,
    testCommands: init.testCommands,
    platform: init.platform,
    sandboxExecPath: init.sandboxExecPath,
    coordinator: init.coordinator,
    coordinationWaitMs: init.coordinationWaitMs,
    commands,
    root() {
      commands.assertOpen();
      root ??= canonicalRoot(init.workspaceRoot).then(async (path) => {
        const startup = (await init.startupCleanup) ?? [];
        for (const result of startup) {
          commands.record(result);
          if (!init.coordinator.quarantineWorkspace)
            throw new CodingCleanupError(startup);
          await init.coordinator.quarantineWorkspace(path, result);
        }
        const cleanup =
          (await init.coordinator.reconcileWorkspace?.(path)) ?? [];
        for (const result of cleanup) commands.record(result);
        return path;
      });
      return root;
    },
    scratch() {
      commands.assertOpen();
      scratch ??= mkdtemp(join(realpathSync(tmpdir()), "agentkit-coding-"));
      return scratch;
    },
    drain: () => commands.drain(),
    async dispose() {
      const cleanup = await commands.drain();
      if (cleanup.some((result) => result.state !== "stopped"))
        throw new CodingCleanupError(cleanup);
      const dir = scratch;
      scratch = undefined;
      if (dir) await rm(await dir, { recursive: true, force: true });
    },
  };
}

/** What a tool body returns; `defineTool` shapes it into an `AiToolResult`. */
export interface Outcome {
  ok: boolean;
  /** The text the model sees. */
  text: string;
  summary?: string;
  truncated?: boolean;
  warnings?: string[];
  /** Structured details for UIs; the model sees `text`. */
  details?: Record<string, unknown>;
}

export interface ToolSpec {
  name: string;
  effect: "read" | "write";
  capability: string;
  description: string;
  inputSchema: AiJsonSchemaObject;
  timeoutMs: number;
}

export type ToolBody = (
  ctx: AiToolExecutionContext,
  input: unknown,
) => Promise<Outcome>;

export function failure(error: CodingError): Outcome {
  return {
    ok: false,
    text: `${error.code}: ${error.message}`,
    summary: `${error.code}: ${error.message}`,
  };
}

/**
 * Wraps a tool body so user errors become `ok:false` results and never throw.
 * An unexpected error is reported the same way with code `io_error`.
 */
export function defineTool(spec: ToolSpec, body: ToolBody): AiTool {
  const definition: AiToolDefinition = {
    name: spec.name,
    version: "1",
    effect: spec.effect,
    capability: spec.capability,
    description: spec.description,
    inputSchema: spec.inputSchema,
    timeoutMs: spec.timeoutMs,
  };
  return {
    definition,
    async execute(ctx, input) {
      let outcome: Outcome;
      try {
        outcome = await body(ctx, input);
      } catch (error) {
        outcome = failure(toCodingError(error, ctx.signal));
      }
      const result: AiToolResult<unknown> = {
        ok: outcome.ok,
        data: outcome.details ?? outcome.text,
        modelData: outcome.text,
        sources: [],
        warnings: outcome.warnings ?? [],
        truncated: outcome.truncated ?? false,
        limits: ctx.limits,
        ...(outcome.summary !== undefined ? { summary: outcome.summary } : {}),
      };
      return result;
    },
  };
}

function toCodingError(error: unknown, signal?: AbortSignal): CodingError {
  if (error instanceof CodingError) return error;
  if (signal?.aborted)
    return new CodingError("aborted", "the call was aborted");
  const message = error instanceof Error ? error.message : String(error);
  return new CodingError("io_error", message.slice(0, 500));
}
