import type { AiRunEvent } from "@agentkit/contracts";
import type { RunChatResult } from "../src/runs/run-loop.js";

/**
 * Drain a `runChat` generator, keeping BOTH halves of its output: the yielded
 * events and the return value. `for await` throws the return value away, which
 * is exactly the mistake this helper exists to prevent in tests.
 */
export async function collectRun(
  gen: AsyncGenerator<AiRunEvent, RunChatResult, unknown>,
): Promise<{ events: AiRunEvent[]; result: RunChatResult }> {
  const events: AiRunEvent[] = [];
  for (;;) {
    const next = await gen.next();
    if (next.done) return { events, result: next.value };
    events.push(next.value);
  }
}
