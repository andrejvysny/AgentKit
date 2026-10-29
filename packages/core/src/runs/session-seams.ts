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
  /** `approval` asks a human to approve this exact call (ADR 0019). Default `input`. */
  kind?: "input" | "approval";
  /** Approval only: sha-256 hex of the call's canonical JSON arguments. */
  argsSha256?: string;
  /** Stable host-generated authorization generation for an uncertain operation. */
  approvalGeneration?: string;
  /** Approval only: host-computed workspace and execution-context binding. */
  contextSha256?: string;
  /**
   * Approval only: the privilege the call asked for (a plan asking for a
   * permission mode, a command asking for a scope). It is RECORDED so the human
   * can see what was really being requested; the host never treats it as granted.
   */
  requestedMode?: string;
  /**
   * Approval only: this call may be covered by a durable, session-scoped
   * standing approval. `digest` is the host-computed key the host will store if
   * the human chooses to remember the approval; `label` is what they are shown.
   * Recorded on the request, never obeyed by the loop.
   */
  standing?: { digest: string; label: string };
}

/** Deterministic, so a restarted attempt derives the same id for the same call. */
export function humanRequestId(
  runId: string,
  toolCallId: string,
  generation?: string,
): string {
  return generation === undefined
    ? `hreq:${runId}:${toolCallId}`
    : `hreq:${runId}:${toolCallId}:generation:${generation}`;
}

/** Error code for a second suspending call in the same step. */
export const SUSPEND_CONFLICT_CODE = "suspend_conflict";
/** Error code for a suspend request with no usable prompt. */
export const SUSPEND_INVALID_CODE = "suspend_invalid";
