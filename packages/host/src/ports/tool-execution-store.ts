import { isAiToolResult, type AiToolResult } from "@agentkit/contracts";
import type { RunWriteFence } from "./conversation-store.js";
import { AgentKitHostError } from "../errors.js";

/** An admission is durable before invoking a tool, even if its body never starts. */
export interface ToolExecutionRecord {
  executionId: string;
  runId: string;
  toolCallId: string;
  toolName: string;
  argsSha256: string;
  contextSha256: string;
  attemptId: string;
  approvalRequestId?: string;
  approvalGeneration?: string;
  status: "admitted" | "completed";
  result?: AiToolResult<unknown>;
}

export type ToolExecutionAdmission = Omit<
  ToolExecutionRecord,
  "status" | "result"
>;

/** Receipts are separate from conversation history and process quarantine. */
export interface ToolExecutionStore {
  /** Atomic, fenced insert. An existing execution or approval returns false. */
  admit(input: ToolExecutionAdmission, fence: RunWriteFence): Promise<boolean>;
  /** Persist the result before delivery to the core loop or UI. */
  complete(
    executionId: string,
    result: AiToolResult<unknown>,
    fence: RunWriteFence,
  ): Promise<void>;
  listByRun(runId: string): Promise<ToolExecutionRecord[]>;
}

function object(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** Corrupt receipts must never turn into absence of uncertain execution. */
export function readToolExecutionRecord(
  value: unknown,
  expected: {
    executionId?: string;
    runId?: string;
    approvalRequestId?: string | null;
  } = {},
): ToolExecutionRecord {
  const fail = (): never => {
    throw new AgentKitHostError(
      "recovery_history_unreadable",
      "A durable execution receipt is malformed; mutations remain blocked.",
    );
  };
  if (!object(value)) return fail();
  for (const key of [
    "executionId",
    "runId",
    "toolCallId",
    "toolName",
    "attemptId",
  ])
    if (typeof value[key] !== "string" || value[key] === "") return fail();
  for (const key of ["argsSha256", "contextSha256"])
    if (typeof value[key] !== "string" || !/^[a-f0-9]{64}$/.test(value[key]))
      return fail();
  for (const key of ["approvalRequestId", "approvalGeneration"])
    if (
      value[key] !== undefined &&
      (typeof value[key] !== "string" || value[key] === "")
    )
      return fail();
  if (value.status !== "admitted" && value.status !== "completed")
    return fail();
  if (value.status === "completed" && !isAiToolResult(value.result))
    return fail();
  if (
    expected.executionId !== undefined &&
    value.executionId !== expected.executionId
  )
    return fail();
  if (expected.runId !== undefined && value.runId !== expected.runId)
    return fail();
  if (
    "approvalRequestId" in expected &&
    value.approvalRequestId !== (expected.approvalRequestId ?? undefined)
  )
    return fail();
  return value as unknown as ToolExecutionRecord;
}
