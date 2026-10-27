import type { AiRunEvent } from "@agentkit/contracts";
import { runAgentLoop } from "./agent-loop.js";
import type { RunChatInput, RunChatResult } from "./run-types.js";

export type { RunChatInput, RunChatResult } from "./run-types.js";

/**
 * Frozen public facade over {@link runAgentLoop}; the loop lives in
 * `agent-loop.ts` (see `docs/adr/0015-unified-agent-runtime.md`). Keep this
 * signature stable.
 */
export function runChat(
  input: RunChatInput,
): AsyncGenerator<AiRunEvent, RunChatResult, unknown> {
  // `suspendable: false` selects the overload whose result cannot be
  // "suspended": a non-suspendable loop never returns it.
  return runAgentLoop({ ...input, suspendable: false });
}
