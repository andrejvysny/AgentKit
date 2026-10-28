import type { AiToolExecutionContext } from "@agentkit/core";
import type { PackEnv } from "./context.js";
import {
  isExecutionLease,
  type ExecutionCleanup,
  type ExecutionIntent,
  type ExecutionLease,
} from "./coordination.js";
import { processStart } from "./coordination-record.js";
import { CodingError } from "./errors.js";
import { runBounded, type RunResult, type RunSpec } from "./process.js";

interface CommandLabel {
  tool: string;
  command: string;
}

async function acquireCommandLease(
  env: PackEnv,
  ctx: AiToolExecutionContext,
  root: string,
  request: CommandLabel,
): Promise<ExecutionLease> {
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
  if (isExecutionLease(lease)) return lease;
  await lease.release();
  throw new CodingError(
    "durable_coordination_required",
    "shell execution requires a durable workspace coordinator",
  );
}

async function executeCommand(
  lease: ExecutionLease,
  intent: ExecutionIntent,
  spec: RunSpec,
): Promise<RunResult> {
  await lease.beginExecution(intent);
  return runBounded({
    ...spec,
    execId: intent.execId,
    onSpawn: async (pid) => {
      const started = await processStart(pid);
      await lease.spawned({
        pid,
        ...(started ? { processStart: started } : {}),
      });
    },
  });
}

interface OwnedOutcome {
  run?: RunResult;
  cleanup: ExecutionCleanup;
  failure?: unknown;
}

async function finishCommand(
  lease: ExecutionLease,
  intent: ExecutionIntent,
  spec: RunSpec,
): Promise<OwnedOutcome> {
  try {
    const run = await executeCommand(lease, intent, spec);
    const cleanup: ExecutionCleanup = {
      executionAttemptId: intent.executionAttemptId,
      state: run.teardown.state,
      survivors: run.teardown.survivors,
    };
    await lease.finishExecution(cleanup);
    return { run, cleanup };
  } catch (failure) {
    return {
      failure,
      cleanup: {
        executionAttemptId: intent.executionAttemptId,
        state: "unknown",
        survivors: [],
        reason: failure instanceof Error ? failure.message : String(failure),
      },
    };
  }
}

export async function runOwnedCommand(
  env: PackEnv,
  ctx: AiToolExecutionContext,
  root: string,
  request: CommandLabel,
  intent: ExecutionIntent,
  spec: RunSpec,
): Promise<RunResult> {
  const lease = await acquireCommandLease(env, ctx, root, request);
  const outcome = await finishCommand(lease, intent, spec);
  try {
    await lease.release();
  } catch (error) {
    if (outcome.cleanup.state === "stopped") {
      outcome.cleanup = {
        ...outcome.cleanup,
        state: "unknown",
        reason: error instanceof Error ? error.message : String(error),
      };
      outcome.failure ??= error;
    }
  }
  env.commands.record(outcome.cleanup);
  if (outcome.failure !== undefined) throw outcome.failure;
  if (!outcome.run)
    throw new CodingError(
      "cleanup_unknown",
      "command produced no cleanup result",
    );
  return outcome.run;
}
