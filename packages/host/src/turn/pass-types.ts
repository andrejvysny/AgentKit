/**
 * The state and the input/result shapes of ONE provider pass, shared by
 * `turn-runner.ts` (which drives passes) and `harness-driver.ts` (which asks
 * for more of them). A separate module so `turn-runner.ts` does not have to
 * export them — `packages/host/src/index.ts` re-exports that file wholesale,
 * and these are internal to the turn pipeline, not part of the package's
 * public surface. This file is deliberately NOT listed in `index.ts`.
 */
import type { AgentSteeringSource } from "@agentkit/core";
import type {
  AiChatMessage,
  AiContextBinding,
  AiToolCall,
  AiToolLimits,
} from "@agentkit/contracts";
import type {
  AiChatRequest,
  AiProviderClient,
  AiToolRegistry,
} from "@agentkit/core";
import type { TaskRecord } from "../ports/task-store.js";
import type { TaskExecutionContext } from "../tasks/task-executor.js";
import type { RunProjectionState } from "./projection.js";
import type { ProviderContinuationSession } from "./provider-continuation.js";
import type { PassTerminal } from "./retry.js";

/**
 * Mutable state accumulated across one provider pass.
 *
 * It is the projection seam's own state — see {@link RunProjectionState} — not
 * a second model beside it: every field this class reads between passes
 * (`content` and `toolCallIds` for the retry decisions, `lastMessageId` for the
 * banners it chains) is one the projector maintains, and two structures
 * tracking the same run would be two answers to "what has this turn written".
 */
export type PassState = RunProjectionState;

export interface PassInput {
  runContent?: AiChatRequest["runContent"];
  providerContinuation?: ProviderContinuationSession;
  task: TaskRecord;
  /** The conversation this turn belongs to, read once from the payload. */
  chatId: string;
  ctx: TaskExecutionContext;
  client: AiProviderClient;
  /** The resolved provider's id — what the usage port bills against. */
  providerId: string;
  model: string;
  messages: AiChatMessage[];
  registry: AiToolRegistry;
  bindings: AiContextBinding[];
  limits: AiToolLimits;
  assistantMessageId: string;
  state: PassState;
  maxToolIterations?: number;
  /** Session main pass only: where durable steers come from. */
  steering?: AgentSteeringSource;
  /** Session main pass only: lets a tool park the run for human input. */
  suspendable?: boolean;
  /** Session main pass only: approved calls to run before the first model request. */
  resumeCalls?: readonly AiToolCall[];
  /** Session main pass only: host-owned tool context (`approvedCalls`). */
  toolMetadata?: Readonly<Record<string, unknown>>;
}

export interface PassResult {
  terminal: PassTerminal;
  failureCode?: string;
  appendedMessages: readonly AiChatMessage[];
}
