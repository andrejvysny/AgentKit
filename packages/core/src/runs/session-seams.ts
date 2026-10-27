import type { AiChatMessage } from "@agentkit/contracts";

/**
 * Opt-in seams that let a host run the loop as a durable session (ADR 0016).
 * Both are inert unless the host passes them: the loop itself never persists
 * anything, it only reports what it applied or why it stopped.
 */

/** One durable user message waiting to be placed into a running loop. */
export interface AgentSteeringInput {
  /** The host's idempotency key; a repeated id is placed at most once. */
  inputId: string;
  /** Must have role "user"; anything else is rejected with a warning. */
  message: AiChatMessage;
}

/**
 * Where steers come from. Polled by the loop at a step boundary only (see
 * `agent-loop.ts`), so a source never races an in-flight provider request.
 */
export interface AgentSteeringSource {
  pending(ctx: {
    runId: string;
    /** The 1-based step the returned inputs would be placed before. */
    step: number;
    signal?: AbortSignal;
  }): Promise<readonly AgentSteeringInput[]>;
}

/** What a tool hands `ctx.suspend` to pause the run for human input. */
export interface AgentSuspendRequest {
  prompt: string;
  /** ISO-8601 expiry of the request. */
  expiresAt?: string;
}

/** Deterministic, so a restarted attempt derives the same id for the same call. */
export function humanRequestId(runId: string, toolCallId: string): string {
  return `hreq:${runId}:${toolCallId}`;
}

/** Error code for a second suspending call in the same step. */
export const SUSPEND_CONFLICT_CODE = "suspend_conflict";
/** Error code for a suspend request with no usable prompt. */
export const SUSPEND_INVALID_CODE = "suspend_invalid";
