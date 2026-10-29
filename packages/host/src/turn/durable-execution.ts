import type { AiToolResult } from "@agentkit/contracts";
import type { AiTool } from "@agentkit/core";
import { AgentKitHostError } from "../errors.js";
import type { AssistantStore } from "../ports/assistant-store.js";
import type { ToolGuardContext } from "../ports/tool-guard.js";
import type { TaskExecutionContext } from "../tasks/task-executor.js";
import { argsSha256Of } from "../sessions/hash.js";

export interface DurableExecution {
  store: AssistantStore;
  ctx: TaskExecutionContext;
  /** Host-owned workspace and execution-policy context. */
  context?: unknown;
  isMutating?: (toolName: string) => boolean;
}

export function executionContextHash(
  input: DurableExecution,
  call: ToolGuardContext,
): Promise<string> {
  return argsSha256Of({
    chatId: call.chatId,
    scopeId: call.scopeId,
    bindings: call.bindings,
    toolName: call.tool.name,
    toolVersion: call.tool.version,
    context: input.context,
  });
}

export async function restoredToolResult(
  input: DurableExecution,
  call: ToolGuardContext,
  argsSha256: string,
): Promise<AiToolResult<unknown> | undefined> {
  const receipts = await input.store.toolExecutions?.listByRun(
    input.ctx.task.taskId,
  );
  const receipt = receipts
    ?.filter((record) => record.toolCallId === call.toolCallId)
    .at(-1);
  if (receipt?.status !== "completed") return undefined;
  if (
    receipt.toolName !== call.tool.name ||
    receipt.argsSha256 !== argsSha256 ||
    receipt.contextSha256 !== (await executionContextHash(input, call))
  )
    throw new AgentKitHostError(
      "execution_context_mismatch",
      "The recovered execution no longer matches its recorded operation or context.",
    );
  if (receipt.result === undefined)
    throw new AgentKitHostError(
      "recovery_history_unreadable",
      "The completed execution has no readable result.",
    );
  return receipt.result;
}

/** No foreign/tool work runs in this transaction. Only a new insert admits it. */
export async function admitToolExecution(
  input: DurableExecution,
  call: ToolGuardContext,
  argsSha256: string,
  approvalRequestId?: string,
  approvalGeneration?: string,
): Promise<string | undefined> {
  if (!input.store.toolExecutions)
    throw new AgentKitHostError(
      "durable_execution_unavailable",
      "This store cannot durably admit mutating tools; execution is blocked.",
    );
  if (!call.toolCallId)
    throw new AgentKitHostError(
      "invalid_execution",
      "Durable tool admission requires a tool-call identity.",
    );
  const executionId = await argsSha256Of({
    runId: input.ctx.task.taskId,
    toolCallId: call.toolCallId,
    approvalRequestId: approvalRequestId ?? "initial",
  });
  const contextSha256 = await executionContextHash(input, call);
  return input.store.transaction(async (tx) => {
    if (approvalRequestId !== undefined) {
      const request = await tx.inputs?.getInput(approvalRequestId);
      const content = request?.content as
        | {
            kind?: string;
            argsSha256?: string;
            toolName?: string;
            approvalGeneration?: string;
            contextSha256?: string;
          }
        | undefined;
      if (
        !request ||
        request.status !== "answered" ||
        !request.appliedMessageId?.startsWith("approval:") ||
        request.targetTaskId !== input.ctx.task.taskId ||
        request.toolCallId !== call.toolCallId ||
        request.sessionId !== call.chatId ||
        content?.kind !== "approval" ||
        content.argsSha256 !== argsSha256 ||
        content.toolName !== call.tool.name ||
        content.approvalGeneration !== approvalGeneration ||
        (content.contextSha256 !== undefined &&
          content.contextSha256 !== contextSha256)
      )
        throw new AgentKitHostError(
          "approval_mismatch",
          "The approval no longer matches this operation, authorization generation or execution context.",
        );
    }
    const admitted = await tx.toolExecutions!.admit(
      {
        executionId,
        runId: input.ctx.task.taskId,
        toolCallId: call.toolCallId!,
        toolName: call.tool.name,
        argsSha256,
        contextSha256,
        attemptId: input.ctx.attemptId,
        ...(approvalRequestId === undefined ? {} : { approvalRequestId }),
        ...(approvalGeneration === undefined ? {} : { approvalGeneration }),
      },
      { taskId: input.ctx.task.taskId, leaseToken: input.ctx.leaseToken },
    );
    return admitted ? executionId : undefined;
  });
}

export async function completeToolExecution(
  input: DurableExecution,
  executionId: string,
  result: Awaited<ReturnType<AiTool["execute"]>>,
): Promise<void> {
  await input.store.toolExecutions!.complete(executionId, result, {
    taskId: input.ctx.task.taskId,
    leaseToken: input.ctx.leaseToken,
  });
}
