import type { AiRunEvent, AiToolCall } from "@agentkit/contracts";
import { nativeAgentLoop } from "./native-loop.js";
import type { AgentLoopHooks } from "./loop-hooks.js";
import type { RunChatInput, RunChatResult } from "./run-types.js";
import type { AgentSteeringSource } from "./session-seams.js";

/**
 * The Rust runtime is the only execution engine. `AGENTKIT_BACKEND` may be
 * unset or `native` (both select Rust); any other value — including the
 * retired `ts` engine selector — fails closed with `invalid_input` rather
 * than silently running something else.
 */
/** Browser-safe environment view: bundlers evaluate this module without a
 * `process`, so the shape is asserted once as a named value, not per read. */
const runtimeProcess: { env?: Record<string, string | undefined> } | undefined =
  (globalThis as { process?: { env?: Record<string, string | undefined> } })
    .process;

function assertNativeBackend(): void {
  const value = runtimeProcess?.env?.AGENTKIT_BACKEND;
  if (value === undefined || value === "native") return;
  throw Object.assign(new Error(`Invalid AGENTKIT_BACKEND: ${value}`), {
    code: "invalid_input",
  });
}

export interface AgentLoopInput extends RunChatInput {
  /** Ordered lifecycle hooks (ADR 0015 section 6). Absent or empty: no cost. */
  hooks?: readonly AgentLoopHooks[];
  /** Per-call hook deadline; non-positive or non-finite disables it. */
  hookTimeoutMs?: number;
  /**
   * Durable user messages placed at step boundaries (ADR 0016 section 3).
   * Absent: the loop never polls, so no extra awaits.
   */
  steering?: AgentSteeringSource;
  /**
   * Lets a tool pause the run for human input via `ctx.suspend` (ADR 0016
   * section 5). Off: tools get no `suspend` and "suspended" is unreachable.
   */
  suspendable?: boolean;
  /**
   * Calls to execute FIRST, before the first model request (ADR 0019): their
   * assistant message is already in `messages`, so the batch pushes none and
   * announces nothing. It counts as step 1 without using an iteration; the
   * first model step is then step 1 too, so `run.usage.step` is unchanged.
   */
  resumeCalls?: readonly AiToolCall[];
  /** Merged into every tool context's `metadata` (host-owned, e.g. approvals). */
  toolMetadata?: Readonly<Record<string, unknown>>;
}
export type AgentLoopResult = Omit<RunChatResult, "terminal"> & {
  terminal: RunChatResult["terminal"] | "suspended";
};

/**
 * Run a multi-turn chat with tool calls. Yields normalized AiRunEvents and
 * returns a {@link RunChatResult}.
 *
 * The execution engine lives in Rust (`agentkit-core` behind
 * `@agentkit/native`): iteration limits, event stamping, tool dispatch,
 * envelopes, budgets, steering, suspension, and provider retries all run
 * there. This function only validates backend selection and hands the input
 * to the bridge, which moves JSON across the boundary and routes the
 * contributed provider/tool/hook callbacks.
 *
 * `input.messages` is never mutated: the loop appends to a private copy and
 * hands back what it appended.
 */
export function runAgentLoop(
  input: AgentLoopInput & { suspendable?: false },
): AsyncGenerator<AiRunEvent, RunChatResult, unknown>;
export function runAgentLoop(
  input: AgentLoopInput,
): AsyncGenerator<AiRunEvent, AgentLoopResult, unknown>;
export function runAgentLoop(
  input: AgentLoopInput,
): AsyncGenerator<AiRunEvent, AgentLoopResult, unknown> {
  assertNativeBackend();
  return nativeAgentLoop(input);
}
