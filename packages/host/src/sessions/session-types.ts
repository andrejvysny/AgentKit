import type { AiContentPart } from "@agentkit/contracts";
import type { AgentInputRecord } from "../ports/agent-input-store.js";
import type { TaskStatus } from "../ports/task-store.js";
import type { AgentSessionDescriptor } from "./descriptor.js";

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
