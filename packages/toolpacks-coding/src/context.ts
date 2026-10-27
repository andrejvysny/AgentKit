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
import { CodingError } from "./errors.js";
import type { CodingLimits } from "./limits.js";
import { canonicalRoot } from "./paths.js";

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
  /** Canonical workspace root (resolved lazily, cached). */
  root(): Promise<string>;
  /** Private scratch directory outside the workspace (created lazily). */
  scratch(): Promise<string>;
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
}

export function createEnv(init: EnvInit): PackEnv {
  let root: Promise<string> | undefined;
  let scratch: Promise<string> | undefined;
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
    root() {
      root ??= canonicalRoot(init.workspaceRoot);
      return root;
    },
    scratch() {
      scratch ??= mkdtemp(join(realpathSync(tmpdir()), "agentkit-coding-"));
      return scratch;
    },
    async dispose() {
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
