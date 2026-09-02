import type { AiToolCall } from "@agentkit/contracts";

/**
 * Make every tool call in one turn answerable, by giving repeats their own id.
 *
 * Ids must be unique within a turn: the assistant message lists every call and
 * each one is answered by a `role:"tool"` message keyed on its id, so a repeat
 * gives two calls one answer, and every projection keyed on `toolCallId`
 * collides. Later duplicates are re-keyed `<id>#2`, `<id>#3` — the id is echoed
 * back on exactly the pair of messages the caller produced, so the re-key never
 * reaches the provider.
 *
 * Shared rather than a step of the OpenAI-compatible client, because the run
 * loop must not depend on WHICH client produced the calls: a third-party
 * `AiProviderClient` that emits two calls under one id would otherwise reach
 * the loop undefended. Running it twice is harmless — the first pass leaves no
 * duplicates for the second to find.
 */
export function dedupeToolCallIds(calls: readonly AiToolCall[]): {
  calls: AiToolCall[];
  duplicateIds: string[];
} {
  const seen = new Map<string, number>();
  const duplicateIds: string[] = [];
  const deduped: AiToolCall[] = [];
  for (const call of calls) {
    const count = (seen.get(call.id) ?? 0) + 1;
    seen.set(call.id, count);
    if (count === 1) {
      deduped.push(call);
      continue;
    }
    if (!duplicateIds.includes(call.id)) duplicateIds.push(call.id);
    deduped.push({ ...call, id: `${call.id}#${count}` });
  }
  return { calls: deduped, duplicateIds };
}
