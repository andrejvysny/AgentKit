/**
 * The state and the input/result shapes of ONE provider pass, shared by
 * `turn-runner.ts` (which drives passes) and `harness-driver.ts` (which asks
 * for more of them). A separate module so `turn-runner.ts` does not have to
 * export them — `packages/host/src/index.ts` re-exports that file wholesale,
 * and these are internal to the turn pipeline, not part of the package's
 * public surface. This file is deliberately NOT listed in `index.ts`.
 */
import type {
  AiChatMessage,
  AiContextBinding,
  AiToolLimits,
} from "@agentkit/contracts";
import type { AiProviderClient, AiToolRegistry } from "@agentkit/core";
import type { TaskRecord } from "../ports/task-store.js";
import type { TaskExecutionContext } from "../tasks/task-executor.js";
import type { RunProjectionState } from "./projection.js";
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
}

export interface PassResult {
  terminal: PassTerminal;
  appendedMessages: readonly AiChatMessage[];
}
