#!/usr/bin/env bun
/**
 * Measures what streaming an answer COSTS THE DATABASE — the placeholder
 * `updateMessage` behind every `run.message.delta` — against a temp-file (not
 * `:memory:`) SQLite store, so the WAL and page-cache behaviour a real desktop
 * deployment pays for is in the number. NOT run in CI (no dev-hardware
 * baseline to compare a runner against); run it by hand:
 *
 *   bun scripts/bench-projection.ts
 *
 * WHAT IT IS FOR. `RunProjector.reflect` used to do one `updateMessage` per
 * delta: a 2000-token answer was ~2000 UPDATEs on ONE row, for a value only
 * the last of which anybody reads. The durable truth of a run is its EVENT
 * LOG, which is appended either way and is untouched by this; the placeholder
 * is a projection of it, so it is safe to let that projection lag by a bounded
 * amount. `projection.ts` now coalesces the write to at most one per 32 deltas
 * or 50 ms, always flushed before any non-delta event (`run.message.completed`
 * and every terminal included) and discarded by `TurnRunner.resetPass`.
 *
 * WHAT IT MEASURES. 2000 deltas through the real sqlite-backed projector,
 * timed end to end, counting the `updateMessage` calls that actually reached
 * the store. `--per-delta` runs the same stream with the coalescing disabled
 * (one write per delta, what this did before), so the two lines can be read
 * side by side without checking out the old code.
 *
 * WHAT IT DOES NOT MEASURE: the event log. This drives `reflect`, not
 * `project`, deliberately — appending 2000 events is the same work in both
 * arms and would swamp the difference the script exists to show.
 */
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { SqliteAssistantStore } from "../packages/adapters-sqlite/src/index.js";
import {
  createRunProjector,
  defaultClock,
  type AssistantStore,
  type RunProjectionContext,
  type TaskRecord,
} from "../packages/host/src/index.js";
import type { AiRunEvent } from "../packages/contracts/src/index.js";

const DELTAS = 2000;
const CHAT_ID = "bench-chat";
const TASK_ID = "bench-task";
const DELTA_TEXT = "token ";

/** A stamped delta event; `reflect` reads only `type` and `data`. */
function delta(seq: number): AiRunEvent {
  return {
    type: "run.message.delta",
    runId: TASK_ID,
    seq,
    eventId: `evt-${seq}`,
    contractVersion: "0.4.0",
    timestamp: "2026-01-01T00:00:00.000Z",
    data: { delta: DELTA_TEXT },
  };
}

async function seed(store: AssistantStore): Promise<{
  ctx: RunProjectionContext;
  assistantMessageId: string;
}> {
  await store.conversations.createChat({ id: CHAT_ID });
  const task: TaskRecord = await store.tasks.createTask({
    taskId: TASK_ID,
    kind: "chat.turn",
    scopeId: CHAT_ID,
    payload: {},
  });
  const question = await store.conversations.appendMessage({
    chatId: CHAT_ID,
    role: "user",
    content: "write me an essay",
  });
  const placeholder = await store.conversations.appendMessage({
    chatId: CHAT_ID,
    runId: TASK_ID,
    role: "assistant",
    content: "",
    parentMessageId: question.id,
    metadata: { placeholder: true },
  });
  return {
    ctx: { task, attemptId: "bench-attempt", leaseToken: "bench-lease" },
    assistantMessageId: placeholder.id,
  };
}

async function run(perDelta: boolean): Promise<void> {
  const dir = mkdtempSync(join(tmpdir(), "agentkit-bench-projection-"));
  const store = new SqliteAssistantStore(join(dir, "bench.db"));
  try {
    const { ctx, assistantMessageId } = await seed(store);

    // Count what actually reached the store, so the two arms are comparable on
    // writes as well as on wall time.
    let writes = 0;
    const conversations = store.conversations;
    const update = conversations.updateMessage.bind(conversations);
    conversations.updateMessage = async (messageId, patch) => {
      writes += 1;
      return update(messageId, patch);
    };

    const projector = createRunProjector({ store, clock: defaultClock });
    const state = projector.createState({
      chatId: CHAT_ID,
      assistantMessageId,
    });

    const started = performance.now();
    for (let seq = 0; seq < DELTAS; seq += 1) {
      await projector.reflect(ctx, state, delta(seq));
      if (perDelta) {
        // The old behaviour, reproduced exactly: force the write the projector
        // would otherwise have coalesced.
        await conversations.updateMessage(assistantMessageId, {
          content: state.content,
        });
        state.unflushedDeltas = 0;
      }
    }
    // The flush every real turn ends with.
    await projector.reflect(ctx, state, {
      type: "run.completed",
      runId: TASK_ID,
      seq: DELTAS,
      eventId: "evt-done",
      contractVersion: "0.4.0",
      timestamp: "2026-01-01T00:00:00.000Z",
      data: { iterations: 1 },
    });
    const elapsed = performance.now() - started;

    const stored = (await store.conversations.listMessages(CHAT_ID)).find(
      (message) => message.id === assistantMessageId,
    );
    const expected = DELTA_TEXT.repeat(DELTAS);
    if (stored?.content !== expected) {
      throw new Error(
        `the stored answer does not match the stream (${String(stored?.content).length} chars vs ${expected.length})`,
      );
    }

    const label = perDelta ? "one write per delta (before)" : "coalesced (now)";
    console.log(
      `${label.padEnd(30)} ${DELTAS} deltas → ${String(writes).padStart(5)} updateMessage calls  ` +
        `${elapsed.toFixed(1).padStart(8)} ms  (${(elapsed / DELTAS).toFixed(3)} ms/delta)`,
    );
  } finally {
    store.close();
    rmSync(dir, { recursive: true, force: true });
  }
}

const perDeltaOnly = process.argv.includes("--per-delta");
console.log(
  `bench-projection — ${DELTAS} deltas through the sqlite-backed RunProjector\n`,
);
if (perDeltaOnly) {
  await run(true);
} else {
  await run(true);
  await run(false);
}
