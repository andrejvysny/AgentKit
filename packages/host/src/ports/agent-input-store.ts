import { AgentInputTransitionError, AgentKitHostError } from "../errors.js";

export type AgentInputKind = "steer" | "follow_up" | "human_request";

export type AgentInputStatus =
  | "pending"
  | "applied"
  | "converted"
  | "answered"
  | "cancelled";

/** What happens to a steer whose target turn ends before it is placed. */
export type AgentInputSettlePolicy = "follow_up" | "cancel";

export interface AgentInputRecord {
  /** Primary key and the caller's idempotency key. */
  inputId: string;
  /** The chat id. */
  sessionId: string;
  kind: AgentInputKind;
  status: AgentInputStatus;
  /** JSON-serializable. */
  content: unknown;
  /** Caller-supplied digest of `content`; how a replay is told from a conflict. */
  contentHash: string;
  /** Store-assigned, strictly increasing insertion order (the FIFO key). */
  order: number;
  targetTaskId?: string;
  /** Steer only. */
  onSettled?: AgentInputSettlePolicy;
  /** Human request: the generation and call an answer must match. */
  attemptId?: string;
  toolCallId?: string;
  expiresAt?: string;
  /** Receipt: where the input landed. */
  appliedTaskId?: string;
  appliedMessageId?: string;
  answerId?: string;
  createdAt: string;
  updatedAt: string;
}

export interface CreateAgentInput {
  inputId: string;
  sessionId: string;
  kind: AgentInputKind;
  content: unknown;
  contentHash: string;
  targetTaskId?: string;
  onSettled?: AgentInputSettlePolicy;
  attemptId?: string;
  toolCallId?: string;
  expiresAt?: string;
}

export interface AgentInputQuery {
  sessionId: string;
  kinds?: readonly AgentInputKind[];
  statuses?: readonly AgentInputStatus[];
  targetTaskId?: string;
}

export interface AgentInputPatch {
  appliedTaskId?: string;
  appliedMessageId?: string;
  answerId?: string;
}

/**
 * Durable inputs that do not become a turn immediately (ADR 0016 §2).
 *
 * Optional on {@link AssistantStore} so it can join the same transaction as
 * tasks and messages without breaking third-party stores.
 */
export interface AgentInputStore {
  /**
   * Idempotent on `inputId`: the same kind, session and `contentHash` returns
   * `{ created: false, record: existing }`; anything differing throws
   * {@link AgentInputConflictError}.
   */
  createInput(
    input: CreateAgentInput,
  ): Promise<{ record: AgentInputRecord; created: boolean }>;
  getInput(inputId: string): Promise<AgentInputRecord | undefined>;
  /** Ordered by `order` ascending. */
  listInputs(query: AgentInputQuery): Promise<AgentInputRecord[]>;
  /**
   * Compare-and-set: throws {@link AgentInputTransitionError} unless the
   * current status is in `from` AND the edge is legal for the record's kind.
   * Writes `patch` and `updatedAt`.
   */
  transitionInput(
    inputId: string,
    from: readonly AgentInputStatus[],
    to: AgentInputStatus,
    patch?: AgentInputPatch,
  ): Promise<AgentInputRecord>;
  /**
   * Remove every input of a session, whatever its status, and return how many
   * rows went. Called inside `ConversationService.deleteChat`'s transaction so
   * a deleted chat leaves no orphaned inputs behind.
   */
  deleteSessionInputs(sessionId: string): Promise<number>;
}

/**
 * Allowed targets from `pending`, per kind. Every non-pending status is
 * terminal, so this is the whole machine.
 */
export const AGENT_INPUT_TRANSITIONS: Readonly<
  Record<AgentInputKind, readonly AgentInputStatus[]>
> = Object.freeze({
  steer: Object.freeze(["applied", "converted", "cancelled"] as const),
  follow_up: Object.freeze(["converted", "cancelled"] as const),
  human_request: Object.freeze(["answered", "cancelled"] as const),
});

/** Shared by adapters so the CAS rule cannot diverge between them. */
export function assertAgentInputTransition(
  record: AgentInputRecord,
  from: readonly AgentInputStatus[],
  to: AgentInputStatus,
): void {
  if (!from.includes(record.status)) {
    throw new AgentInputTransitionError(
      `Input ${record.inputId} is ${record.status}, expected one of [${from.join(", ")}].`,
      { inputId: record.inputId, current: record.status, from, to },
    );
  }
  if (
    record.status !== "pending" ||
    !AGENT_INPUT_TRANSITIONS[record.kind].includes(to)
  ) {
    throw new AgentInputTransitionError(
      `Input ${record.inputId} (${record.kind}) cannot move ${record.status} → ${to}.`,
      {
        inputId: record.inputId,
        kind: record.kind,
        current: record.status,
        to,
      },
    );
  }
}

const KINDS: readonly AgentInputKind[] = [
  "steer",
  "follow_up",
  "human_request",
];

/** Shared create-time validation; throws `invalid_input`. */
export function validateCreateAgentInput(input: CreateAgentInput): void {
  const bad = (message: string): never => {
    throw new AgentKitHostError("invalid_input", message, {
      inputId: input.inputId,
    });
  };
  for (const field of ["inputId", "sessionId", "contentHash"] as const) {
    const value: unknown = input[field];
    if (typeof value !== "string" || value === "")
      bad(`${field} must be a non-empty string.`);
  }
  if (!KINDS.includes(input.kind)) bad(`Unknown input kind: ${input.kind}.`);
  if (input.onSettled !== undefined) {
    if (input.kind !== "steer") bad("onSettled is only valid for steer.");
    if (input.onSettled !== "follow_up" && input.onSettled !== "cancel")
      bad(`Unknown onSettled policy: ${String(input.onSettled)}.`);
  }
  if (input.kind === "human_request") {
    for (const field of ["attemptId", "toolCallId"] as const) {
      const value = input[field];
      if (typeof value !== "string" || value === "")
        bad(`${field} is required for human_request.`);
    }
  } else if (
    input.attemptId !== undefined ||
    input.toolCallId !== undefined ||
    input.expiresAt !== undefined
  ) {
    bad(
      "attemptId, toolCallId and expiresAt are only valid for human_request.",
    );
  }
  if (input.targetTaskId !== undefined && input.targetTaskId === "")
    bad("targetTaskId must be non-empty when present.");
  if (
    input.expiresAt !== undefined &&
    (typeof input.expiresAt !== "string" ||
      Number.isNaN(new Date(input.expiresAt).getTime()))
  )
    bad("expiresAt must be a valid ISO timestamp.");
  if (JSON.stringify(input.content) === undefined)
    bad("content must be JSON-serializable.");
}
