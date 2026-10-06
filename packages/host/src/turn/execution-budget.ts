import {
  ExecutionBudget,
  type ExecutionBudgetState,
  type ExecutionBudgets,
} from "@agentkit/core";
import type { AssistantStore } from "../ports/assistant-store.js";
import type { TaskExecutionContext } from "../tasks/task-executor.js";

/** Stored in task progress, never reset by a retry or correction pass. */
export async function createTurnBudget(
  store: AssistantStore,
  ctx: TaskExecutionContext,
  limits: ExecutionBudgets,
): Promise<ExecutionBudget> {
  const saved = ctx.task.progress?.executionBudget;
  const budget = new ExecutionBudget(limits, {
    signal: ctx.signal,
    ...(saved === undefined ? {} : { state: saved as ExecutionBudgetState }),
    checkpoint: async (state) => {
      await store.transaction(async (tx) => {
        const task = await tx.tasks.getTask(ctx.task.taskId);
        await tx.tasks.updateProgress(
          ctx.task.taskId,
          { ...task?.progress, executionBudget: state },
          { leaseToken: ctx.leaseToken },
        );
      });
    },
  });
  try {
    await store.transaction(async (tx) => {
      const task = await tx.tasks.getTask(ctx.task.taskId);
      await tx.tasks.updateProgress(
        ctx.task.taskId,
        { ...task?.progress, executionBudget: budget.snapshot() },
        { leaseToken: ctx.leaseToken },
      );
    });
    if (ctx.task.attemptCount > 1) await budget.consume("retries");
    return budget;
  } catch (error) {
    budget.dispose();
    throw error;
  }
}
