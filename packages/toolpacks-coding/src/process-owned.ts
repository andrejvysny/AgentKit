import type { AiToolExecutionContext } from "@agentkit/core";
import type { PackEnv } from "./context.js";
import {
  isExecutionLease,
  type ExecutionCleanup,
  type ExecutionIntent,
  type ExecutionLease,
} from "./coordination.js";
import { CodingError } from "./errors.js";
import { runBounded, type RunResult, type RunSpec } from "./process.js";
import type { ProcessDiscovery } from "./process-identity.js";

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
  if (isExecutionLease(lease)) {
    try {
      const revalidate = ctx.metadata?.["revalidateExecutionAuthorization"];
      if (typeof revalidate === "function" && (await revalidate()) !== true)
        throw new Error("authorization changed");
      return lease;
    } catch {
      await lease.release();
      throw new CodingError(
        "approval_context_changed",
        "Command authorization changed while waiting for workspace access; approve the updated command context.",
      );
    }
  }
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
  let persistence = Promise.resolve();
  const persist = (write: () => Promise<void>): Promise<void> => {
    persistence = persistence.then(write);
    return persistence;
  };
  return runBounded({
    ...spec,
    execId: intent.execId,
    onSpawn: (pid, identity, guardianProtocol) =>
      persist(() =>
        lease.spawned({
          pid,
          ...(identity ? { startIdentity: identity.startIdentity } : {}),
          ...(guardianProtocol ? { guardianProtocol } : {}),
          ...(guardianProtocol && lease.admitted
            ? { admission: "pending" as const }
            : {}),
        }),
      ),
    ...(lease.admitted
      ? { onAdmit: () => persist(() => lease.admitted!()) }
      : {}),
    onDiscovery: (discovery: ProcessDiscovery & { closed?: boolean }) =>
      persist(async () => {
        await lease.observed?.(
          discovery.identities,
          discovery.complete,
          discovery.closed,
          discovery.sealed,
        );
      }),
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
