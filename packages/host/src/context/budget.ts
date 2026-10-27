/**
 * Context budgeting for agent-session turns (ADR 0017 part 1): where the
 * window size comes from, and how a transcript is made to fit it.
 *
 * Session transcripts replay every prior tool call and result, so they grow
 * without bound; the budget turns "the provider rejects the request" into a
 * deliberate, diagnosed trim of the OLDEST whole turns, or a refusal when even
 * the current turn cannot fit. Non-session chats never reach this module.
 */
import type {
  AiChatMessage,
  AiProviderCapabilities,
  AiProviderModel,
} from "@agentkit/contracts";
import type { TokenEstimator } from "@agentkit/core";
import type { AgentSessionDescriptor } from "../sessions/descriptor.js";

/** Used only when nothing else says how large the window is. */
export const FALLBACK_CONTEXT_WINDOW_TOKENS = 32768;
/** Tokens kept free for the reply; matches Pi's default. */
export const DEFAULT_RESERVE_TOKENS = 16384;

export type ContextWindowSource =
  | "descriptor"
  | "model"
  | "capabilities"
  | "fallback";

export interface ResolvedContextWindow {
  windowTokens: number;
  source: ContextWindowSource;
}

export interface ResolveContextWindowInput {
  descriptor: Pick<AgentSessionDescriptor, "context"> | undefined;
  /** The stored catalogue entry for the model in use, when there is one. */
  model: Pick<AiProviderModel, "contextWindowTokens"> | undefined;
  capabilities: Pick<AiProviderCapabilities, "maxContextTokens"> | null;
}

function positive(value: number | undefined): value is number {
  return value !== undefined && Number.isFinite(value) && value > 0;
}

/**
 * Descriptor override, then the model's own advertised window, then the
 * provider-wide capability, then a conservative guess. Only the guess is
 * flagged: it is the one case where the budget may be badly wrong, so the
 * caller warns about it.
 */
export function resolveContextWindow(
  input: ResolveContextWindowInput,
): ResolvedContextWindow {
  const override = input.descriptor?.context?.windowTokens;
  if (positive(override))
    return { windowTokens: override, source: "descriptor" };
  const advertised = input.model?.contextWindowTokens;
  if (positive(advertised))
    return { windowTokens: advertised, source: "model" };
  const capability = input.capabilities?.maxContextTokens;
  if (positive(capability))
    return { windowTokens: capability, source: "capabilities" };
  return {
    windowTokens: FALLBACK_CONTEXT_WINDOW_TOKENS,
    source: "fallback",
  };
}

/** The reserve, never more than half the window so a small window stays usable. */
export function resolveReserveTokens(
  windowTokens: number,
  requested: number | undefined,
): number {
  const reserve = positive(requested) ? requested : DEFAULT_RESERVE_TOKENS;
  return Math.min(reserve, Math.floor(windowTokens / 2));
}

export interface FitToBudgetInput {
  windowTokens: number;
  reserveTokens: number;
  /** Tokens the tool definitions cost on every request. */
  toolTokens: number;
  /** Other fixed cost (run-content messages added after fitting). */
  extraTokens?: number;
  estimator: TokenEstimator;
}

export interface FitToBudgetResult {
  messages: AiChatMessage[];
  droppedTurns: number;
  estimatedTokens: number;
  budgetTokens: number;
  overflow: boolean;
}

/**
 * Whether the message at `index` begins a turn: a user message that does not
 * directly follow a tool result. A steer is placed between two model steps,
 * i.e. right after a tool batch, so it continues its turn; a real user message
 * follows the previous turn's final answer.
 */
function startsTurn(
  messages: readonly AiChatMessage[],
  index: number,
): boolean {
  const message = messages[index];
  if (message?.role !== "user") return false;
  return messages[index - 1]?.role !== "tool";
}

/**
 * Drop the oldest whole turns until the messages fit the window less the
 * reserve and the tool definitions.
 *
 * A turn is a user message and everything up to the next turn start, so a
 * tool call and its results are always dropped together. The leading system
 * message and the last turn (the current one, with the run's own records) are
 * never dropped; when they alone are over budget the result says `overflow`.
 */
export function fitToBudget(
  messages: readonly AiChatMessage[],
  input: FitToBudgetInput,
): FitToBudgetResult {
  const { estimator } = input;
  const budgetTokens =
    input.windowTokens -
    input.reserveTokens -
    input.toolTokens -
    (input.extraTokens ?? 0);
  const costs = messages.map((message) => estimator.estimateMessage(message));
  let total = costs.reduce((sum, cost) => sum + cost, 0);

  const bodyStart = messages[0]?.role === "system" ? 1 : 0;
  const starts: number[] = [];
  for (let index = bodyStart; index < messages.length; index += 1) {
    if (startsTurn(messages, index)) starts.push(index);
  }
  // Anything before the first turn start (a window cut mid-turn) is a
  // droppable turn of its own; the last turn is always kept.
  const firstStart = starts[0];
  const boundaries =
    firstStart === undefined || firstStart > bodyStart
      ? [bodyStart, ...starts]
      : starts;

  let dropUntil = bodyStart;
  let droppedTurns = 0;
  while (total > budgetTokens && droppedTurns < boundaries.length - 1) {
    const next = boundaries[droppedTurns + 1] as number;
    for (let index = dropUntil; index < next; index += 1) {
      total -= costs[index] as number;
    }
    dropUntil = next;
    droppedTurns += 1;
  }

  return {
    messages: [...messages.slice(0, bodyStart), ...messages.slice(dropUntil)],
    droppedTurns,
    estimatedTokens: total,
    budgetTokens,
    overflow: total > budgetTokens,
  };
}
