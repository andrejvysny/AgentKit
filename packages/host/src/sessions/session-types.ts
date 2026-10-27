import type { AiContentPart } from "@agentkit/contracts";
import type { AgentInputRecord } from "../ports/agent-input-store.js";
import type { TaskStatus } from "../ports/task-store.js";
import type { AgentSessionDescriptor } from "./descriptor.js";
import type { SessionExecutionState } from "./plan-mode.js";

/** What a caller may send as a message body. */
export type SessionContent = string | AiContentPart[];

export interface AgentSessionState {
  sessionId: string;
  descriptor: AgentSessionDescriptor;
  /** The unfinished `chat.turn` in the session's scope, if any. */
  activeTurn?: { turnId: string; status: TaskStatus };
  /** Queued steers and follow-ups, oldest first. Human requests are in `waiting`. */
  pending: AgentInputRecord[];
  /** The oldest unanswered human request. */
  waiting?: AgentInputRecord;
  /** Set when `waiting` asks to approve one exact tool call (ADR 0019). */
  waitingApproval?: {
    requestId: string;
    toolCallId: string;
    toolName: string;
    argsSha256: string;
    prompt: string;
  };
  /**
   * Set when the pending approval is a PLAN approval (ADR 0019 part 2): the
   * session is blocked until a human approves the plan as a whole.
   */
  waitingPlan?: {
    requestId: string;
    toolName: string;
    planDigest: string;
    plan?: string;
    requestedMode?: string;
  };
  /** The session's durable execution state, when one was ever set. */
  execution?: SessionExecutionState;
}

export type AgentSendResult =
  | {
      kind: "turn";
      turnId: string;
      userMessageId: string;
      assistantMessageId: string;
    }
  /** A follow-up or steer was stored for later. */
  | { kind: "queued"; input: AgentInputRecord };

export interface AgentAnswerReceipt {
  requestId: string;
  answerId: string;
  turnId: string;
  messageId: string;
  /** True when this exact answer had already been recorded. */
  duplicate: boolean;
}

export interface CreateSessionInput {
  sessionId?: string;
  title?: string;
  profile: { id: string; digest?: string };
  providerId?: string;
  model?: string;
  /** Project instructions root (ADR 0017). */
  workspace?: { root: string; trusted: boolean };
  /** Context policy and budget overrides (ADR 0017). */
  context?: {
    policy?: "final_answers" | "transcript";
    windowTokens?: number;
    reserveTokens?: number;
    keepRecentTokens?: number;
  };
  /** Skills re-selected on every turn. */
  skills?: { id: string; revision: string }[];
  /**
   * Initial permission mode (e.g. `"plan"` to start read-only, or `"read_only"`
   * for a constrained role). Absent means the host's configured default.
   */
  executionMode?: string;
  metadata?: Record<string, unknown>;
}

export interface SendInput {
  content: SessionContent;
  /** Idempotency key; required for `whenBusy: "follow_up" | "steer"`. */
  inputId?: string;
  whenBusy?: "reject" | "follow_up" | "steer";
}

export interface FollowUpInput {
  inputId: string;
  content: SessionContent;
}

export interface SteerInput {
  inputId: string;
  content: SessionContent;
  /** Defaults to the active turn. */
  turnId?: string;
  /** What happens if the target ends before the steer is placed. */
  onSettled?: "follow_up" | "cancel";
}

export interface AnswerInput {
  requestId: string;
  answerId: string;
  response: unknown;
}

/** One `chat.turn` of a session, as {@link AgentSessionService.listTurns} reports it. */
export interface AgentTurnSummary {
  turnId: string;
  status: TaskStatus;
  /** Attempts made so far; an interrupted-and-resumed turn counts each one. */
  attempts: number;
  /** When the turn was enqueued. */
  createdAt: string;
  finishedAt?: string;
  model?: string;
  providerId?: string;
  error?: string;
}

/** Provider-reported usage of one turn, summed over its settled requests. */
export interface AgentTurnUsage {
  turnId: string;
  requests: number;
  promptTokens: number;
  completionTokens: number;
  totalTokens: number;
  /** Prompt tokens of the turn's newest request that reported any. */
  lastPromptTokens?: number;
}

export interface AgentSessionUsage {
  sessionId: string;
  turns: AgentTurnUsage[];
  totals: {
    requests: number;
    promptTokens: number;
    completionTokens: number;
    totalTokens: number;
  };
  /** Window the next request is budgeted against (descriptor, then model, then fallback). */
  contextWindowTokens: number;
  contextWindowSource: "descriptor" | "model" | "capabilities" | "fallback";
  /** Provider-reported context fill of the newest request, when any reported. */
  lastPromptTokens?: number;
}

/** One active-path message in an {@link AgentSessionExport}. */
export interface AgentExportMessage {
  id: string;
  role: "user" | "assistant" | "tool" | "system";
  content: unknown;
  toolCalls?: unknown[];
  toolCallId?: string;
  runId?: string;
  /** Only the allow-listed keys; see `EXPORT_METADATA_KEYS` in session-report.ts. */
  metadata: Record<string, unknown>;
  createdAt: string;
}

/** A self-contained, JSON-serializable history of a session. */
export interface AgentSessionExport {
  format: "agentkit.session.export";
  version: 1;
  exportedAt: string;
  sessionId: string;
  descriptor: AgentSessionDescriptor;
  execution?: SessionExecutionState;
  messages: AgentExportMessage[];
  turns: AgentTurnSummary[];
  /** Every steer, follow-up and human request, approvals with status and answer. */
  inputs: AgentInputRecord[];
  /** Derived context summaries, when the store keeps them. */
  summaries?: {
    summaryId: string;
    coversThroughMessageId: string;
    firstKeptMessageId: string;
    summary: string;
    model: string;
    providerId?: string;
    createdAt: string;
  }[];
}
