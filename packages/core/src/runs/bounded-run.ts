import type { AiRunEvent } from "@agentkit/contracts";
import { AiToolRegistry } from "../tools/registry.js";
import { createEventStamper } from "../events.js";
import { newRunId, nowIso } from "../ids.js";
import { ExecutionBudget, ExecutionBudgetError } from "./execution-budget.js";
import type { RunChatInput, RunChatResult } from "./run-loop.js";
import { boundedClient } from "./bounded-provider.js";

function boundedRegistry(
  registry: AiToolRegistry,
  budget: ExecutionBudget,
): AiToolRegistry {
  const result = new AiToolRegistry();
  for (const definition of registry.listDefinitions()) {
    const tool = registry.get(definition.name);
    if (!tool) continue;
    result.register({
      definition,
      async execute(context, input) {
        await budget.consume("toolCalls");
        return budget.race(
          () => tool.execute(context, input),
          budget.limits.toolMs,
          "tool",
        );
      },
    });
  }
  return result;
}

export async function* boundedRun(
  input: RunChatInput,
  run: (
    input: RunChatInput,
  ) => AsyncGenerator<AiRunEvent, RunChatResult, unknown>,
): AsyncGenerator<AiRunEvent, RunChatResult, unknown> {
  const budget =
    input.executionBudget ??
    new ExecutionBudget(input.budgets, { signal: input.signal });
  const runId = input.runId ?? newRunId();
  let seq = input.firstSeq ?? 0;
  const iterator = run({
    ...input,
    runId,
    signal: budget.signal,
    client: boundedClient(input.client, budget),
    registry: boundedRegistry(input.registry, budget),
  });
  try {
    for (;;) {
      const next = await iterator.next();
      if (budget.signal.reason instanceof ExecutionBudgetError)
        throw budget.signal.reason;
      if (next.done) return next.value;
      seq = next.value.seq + 1;
      yield next.value;
    }
  } catch (error) {
    const failure = failedRun(input, budget, runId, seq, error);
    yield failure.event;
    return {
      runId,
      terminal: failure.terminal,
      appendedMessages: [],
      iterations: budget.snapshot().providerRequests,
    };
  } finally {
    void iterator
      .return({
        runId,
        terminal: "cancelled",
        appendedMessages: [],
        iterations: 0,
      })
      .catch(() => {});
    if (!input.executionBudget) budget.dispose();
  }
}

function failedRun(
  input: RunChatInput,
  budget: ExecutionBudget,
  runId: string,
  seq: number,
  error: unknown,
): { event: AiRunEvent; terminal: RunChatResult["terminal"] } {
  const failure = error instanceof ExecutionBudgetError;
  const cancelled = budget.signal.aborted && !failure;
  const stamp = createEventStamper({
    firstSeq: seq,
    attemptId: input.attemptId,
  });
  const event = stamp(
    cancelled
      ? {
          type: "run.cancelled",
          runId,
          timestamp: nowIso(),
          data: { reason: "aborted" },
        }
      : {
          type: "run.failed",
          runId,
          timestamp: nowIso(),
          data: {
            errorCode: failure ? error.code : "provider_error",
            errorMessage: failure ? error.message : "Execution failed",
          },
        },
  );
  return { event, terminal: cancelled ? "cancelled" : "failed" };
}
