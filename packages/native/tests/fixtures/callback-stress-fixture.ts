import { testServiceOptions } from "../helpers/test-profiles.js";
/**
 * Run separately because a synchronous claim/resolve deadlock also freezes
 * JavaScript timers. The parent owns both the heartbeat and wall-clock bounds.
 */
import assert from "node:assert/strict";
import {
  NativeStore,
  claimCall,
  resolveCall,
  type SessionService,
} from "../../generated/index.js";

const BATCH_SIZE = 32;
const BATCHES = 100;
const emit = (value: Record<string, unknown>): void => {
  console.log(JSON.stringify(value));
};

emit({ kind: "heartbeat" });
if (process.argv.includes("--freeze")) {
  Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0);
  throw new Error("freeze unexpectedly ended");
}

let phase: "normal" | "cancel" | "close" = "normal";
let entries = 0;
let duplicates = 0;
let cancellations = 0;
let settled = 0;
let maxPending = 0;
const entered = new Set<string>();
const pending = new Set<string>();
const held = new Set<string>();
const failures: string[] = [];
const heartbeat = setInterval(() => emit({ kind: "heartbeat", entries }), 50);

function receive(error: Error | null, raw: string): void {
  let callId: string | undefined;
  try {
    assert.equal(error, null);
    const envelope = JSON.parse(raw) as { kind: string; callId: string };
    callId = envelope.callId;
    if (envelope.kind === "cancel" || envelope.kind === "discard") {
      cancellations += 1;
      pending.delete(callId);
      assert.equal(claimCall(callId), false);
      return;
    }
    assert.equal(envelope.kind, "provider");
    assert.equal(claimCall(callId), true);
    assert.equal(claimCall(callId), false, "a live claim must be single-use");
    if (entered.has(callId)) duplicates += 1;
    entered.add(callId);
    entries += 1;
    pending.add(callId);
    maxPending = Math.max(maxPending, pending.size);
    if (phase !== "normal") {
      held.add(callId);
      return;
    }
    const id = callId;
    // Yield so concurrent requests race synchronous claims and resolves.
    setTimeout(() => settle(id), entries % 3);
  } catch (error) {
    failures.push(String(error));
    if (callId !== undefined) resolveCall(callId, null, String(error));
  }
}

function settle(callId: string): void {
  try {
    const result = JSON.stringify({ content: "done", finishReason: "stop" });
    assert.equal(resolveCall(callId, result, null), true);
    assert.equal(resolveCall(callId, result, null), false);
    assert.equal(claimCall(callId), false);
    settled += 1;
    pending.delete(callId);
  } catch (error) {
    failures.push(String(error));
  }
}

async function waitFor(condition: () => boolean): Promise<void> {
  const deadline = Date.now() + 15_000;
  while (!condition()) {
    assert.equal(failures.length, 0, failures.join("\n"));
    assert.ok(Date.now() < deadline, "callback progress exceeded 15 seconds");
    await Bun.sleep(2);
  }
}

async function startBatch(
  service: SessionService,
  sessions: string[],
): Promise<{ sessionId: string; turnId: string }[]> {
  return Promise.all(
    sessions.map(async (sessionId) => {
      const result = JSON.parse(
        await service.send(sessionId, JSON.stringify({ content: "go" })),
      ) as { kind: string; turnId: string };
      assert.equal(result.kind, "turn");
      assert.equal(typeof result.turnId, "string");
      return { sessionId, turnId: result.turnId };
    }),
  );
}

async function drain(
  service: SessionService,
  turns: { sessionId: string; turnId: string }[],
  expectedStatus: string,
): Promise<void> {
  await Promise.all(
    turns.map(async ({ turnId }) => {
      const subscription = await service.subscribeTurn(turnId, 0);
      const deadline = Date.now() + 15_000;
      let status: string | undefined;
      try {
        for (;;) {
          const batch = JSON.parse(await subscription.poll(64, 25)) as {
            done: boolean;
            events: { type: string; task?: { status: string } }[];
          };
          for (const event of batch.events) {
            if (event.type === "settled") status = event.task?.status;
          }
          if (batch.done) break;
          assert.ok(
            Date.now() < deadline,
            "turn settlement exceeded 15 seconds",
          );
        }
        assert.equal(status, expectedStatus);
      } finally {
        subscription.cancel();
      }
    }),
  );
}

function assertStale(): void {
  for (const callId of held) {
    assert.equal(claimCall(callId), false);
    assert.equal(resolveCall(callId, "{}", null), false);
    pending.delete(callId);
  }
  held.clear();
}

async function createSessions(service: SessionService): Promise<string[]> {
  return Promise.all(
    Array.from({ length: BATCH_SIZE }, async (_, index) => {
      const sessionId = `callback-stress-${index}`;
      await service.createSession(
        JSON.stringify({ sessionId, profileId: "stress", model: "test-model" }),
      );
      return sessionId;
    }),
  );
}

async function normalBatches(
  service: SessionService,
  sessions: string[],
): Promise<void> {
  for (let batch = 0; batch < BATCHES; batch += 1) {
    await drain(service, await startBatch(service, sessions), "completed");
    assert.equal(entries, (batch + 1) * BATCH_SIZE);
    assert.equal(pending.size, 0);
  }
}

async function cancelBatch(
  service: SessionService,
  sessions: string[],
): Promise<void> {
  phase = "cancel";
  const cancelled = await startBatch(service, sessions);
  await waitFor(() => held.size === BATCH_SIZE);
  await Promise.all(
    cancelled.map(({ sessionId, turnId }) => service.cancel(sessionId, turnId)),
  );
  await drain(service, cancelled, "cancelled");
  await waitFor(() => cancellations >= BATCH_SIZE);
  assertStale();
}

async function closeBatch(
  service: SessionService,
  sessions: string[],
): Promise<number> {
  phase = "close";
  await startBatch(service, sessions);
  await waitFor(() => held.size === BATCH_SIZE);
  const beforeClose = entries;
  const closeStarted = Date.now();
  service.detach();
  await Promise.all(Array.from({ length: BATCH_SIZE }, () => service.close()));
  assert.equal(service.closeUncertain, false);
  assertStale();
  await Bun.sleep(20);
  assert.equal(entries, beforeClose, "close must dispatch no fresh callbacks");
  return Date.now() - closeStarted;
}

async function run(): Promise<void> {
  const store = NativeStore.memory();
  const service = await store.createService(
    receive,
    JSON.stringify(
      testServiceOptions(
        JSON.parse(
          JSON.stringify({
            subagentMaxConcurrency: 20,
            closeTimeoutMs: 2_000,
          }) ?? "{}",
        ),
      ),
    ),
  );
  service.bindEnv();
  try {
    const sessions = await createSessions(service);
    await normalBatches(service, sessions);
    await cancelBatch(service, sessions);
    const closeMs = await closeBatch(service, sessions);
    assert.equal(entries, BATCH_SIZE * (BATCHES + 2));
    assert.equal(settled, BATCH_SIZE * BATCHES);
    assert.equal(duplicates, 0);
    assert.equal(pending.size, 0);
    assert.equal(failures.length, 0, failures.join("\n"));
    emit({
      kind: "result",
      batches: BATCHES,
      batchSize: BATCH_SIZE,
      entries,
      settled,
      duplicates,
      cancelled: BATCH_SIZE,
      closeCalls: BATCH_SIZE,
      closeMs,
      maxPending,
    });
  } finally {
    service.detach();
    await service.close();
    await store.close();
  }
}

try {
  await run();
} finally {
  clearInterval(heartbeat);
}
