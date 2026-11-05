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
  /** `resolved`: an operator closed an uncertain receipt without a result. */
  status: "admitted" | "completed" | "resolved";
  result?: AiToolResult<unknown>;
  /** Audit of an explicit operator resolution; never written by a run. */
  resolution?: ToolExecutionResolution;
}

export interface ToolExecutionResolution {
  outcome: "completed" | "not_started" | "acknowledged";
  note?: string;
  resolvedAt: number;
  resolvedBy?: string;
}

const RESOLUTION_OUTCOMES = ["completed", "not_started", "acknowledged"];

export type ToolExecutionAdmission = Omit<
  ToolExecutionRecord,
  "status" | "result" | "resolution"
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
  if (
    value.status !== "admitted" &&
    value.status !== "completed" &&
    value.status !== "resolved"
  )
    return fail();
  if (value.status === "completed" && !isAiToolResult(value.result))
    return fail();
  if (value.resolution !== undefined) {
    const resolution = value.resolution;
    if (
      !object(resolution) ||
      typeof resolution.outcome !== "string" ||
      !RESOLUTION_OUTCOMES.includes(resolution.outcome) ||
      typeof resolution.resolvedAt !== "number" ||
      (resolution.note !== undefined && typeof resolution.note !== "string") ||
      (resolution.resolvedBy !== undefined &&
        typeof resolution.resolvedBy !== "string")
    )
      return fail();
  }
  // A resolved receipt carries a non-completed resolution and no result.
  if (
    value.status === "resolved" &&
    (value.result !== undefined ||
      !object(value.resolution) ||
      value.resolution.outcome === "completed")
  )
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
