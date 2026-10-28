#!/usr/bin/env node
/**
 * Packed-consumer acceptance scenario for `@agentkit/native`.
 *
 * Runs OUTSIDE the repository, against the INSTALLED package (resolved from
 * node_modules by package name), under both Bun and Node, with no Rust
 * toolchain on PATH. The gate (`check-native-package.mjs`) drives two modes
 * as SEPARATE child processes:
 *
 *   consumer.mjs run <dbPath>    full lifecycle: open file store → create
 *                                service/session → send → committed events →
 *                                approval wait (auto_edit command MUST ask —
 *                                shared permission matrix through the
 *                                installed artifact) → answer → resume →
 *                                terminal receipt → cancel a live turn →
 *                                awaited close, no fabricated effects.
 *   consumer.mjs reopen <dbPath> FRESH-PROCESS reopen of the same database:
 *                                durable replay with NO tool registered —
 *                                a re-dispatch would strand an unsettled
 *                                callback, and completion text must be
 *                                present from durable state alone.
 */
import { rmSync } from "node:fs";
import { bindingId, NativeStore } from "@agentkit/native";

function assert(condition, message) {
  if (!condition) throw new Error("assertion failed: " + message);
}

async function waitFor(read, accept, timeoutMs = 20000) {
  const started = Date.now();
  let value = await read();
  while (!accept(value) && Date.now() - started < timeoutMs) {
    await new Promise((resolve) => setTimeout(resolve, 20));
    value = await read();
  }
  assert(accept(value), "condition not reached within " + timeoutMs + "ms");
  return value;
}

/** Drain a turn to its terminal update; returns events + every task status
 * seen along the way. */
async function pollToDone(service, turnId, timeoutMs = 20000) {
  const subscription = await service.subscribeTurn(turnId, 0);
  const events = [];
  const statuses = [];
  let done = false;
  const started = Date.now();
  while (Date.now() - started < timeoutMs) {
    const batch = await subscription.poll(64, 100);
    for (const update of batch.events) {
      events.push(update);
      const task = update && update.task;
      if (task && typeof task.status === "string") statuses.push(task.status);
    }
    done = batch.done;
    if (done) break;
  }
  subscription.cancel();
  return { events, statuses, done };
}

const mode = process.argv[2];
const dbPath = process.argv[3];
assert(
  bindingId === "agentkit-native",
  "binding identity probe returned " + String(bindingId),
);
assert(dbPath, "usage: consumer.mjs <run|reopen> <dbPath>");

const runtime = process.versions.bun
  ? "bun " + process.versions.bun
  : "node " + process.version;

if (mode === "run") {
  rmSync(dbPath, { force: true });
  rmSync(dbPath + "-wal", { force: true });
  rmSync(dbPath + "-shm", { force: true });
  const store = await NativeStore.open(dbPath);
  const service = await store.createService({ closeTimeoutMs: 5000 });
  let providerCalls = 0;
  let releaseSlow;
  const slowGate = new Promise((resolve) => {
    releaseSlow = resolve;
  });
  let commandEffects = 0;
  let commandInput = null;
  service.setProvider(async () => {
    providerCalls += 1;
    if (providerCalls === 1) {
      return {
        content: "",
        finishReason: "tool_calls",
        toolCalls: [
          { id: "call-1", name: "run_cmd", arguments: { command: "ls -la" } },
        ],
      };
    }
    if (providerCalls === 2) {
      return {
        content: "approved and done",
        finishReason: "stop",
        toolCalls: [],
      };
    }
    if (providerCalls === 3) {
      return {
        content: "",
        finishReason: "tool_calls",
        toolCalls: [
          { id: "call-2", name: "run_slow", arguments: { command: "sleep" } },
        ],
      };
    }
    // Tripwire: the cancelled turn must never drive a fourth request.
    return {
      content: "provider must not be called again",
      finishReason: "stop",
      toolCalls: [],
    };
  });
  await service.registerTool(
    {
      name: "run_cmd",
      effect: "write",
      // Capability-classified Exec: `auto_edit` must ASK for this call —
      // the shared permission matrix, exercised through the installed
      // artifact on the consumer's own runtime.
      capability: "process.exec",
      inputSchema: { type: "object" },
    },
    async (input) => {
      commandEffects += 1;
      commandInput = input;
      return { data: "ok" };
    },
  );
  await service.registerTool(
    {
      name: "run_slow",
      effect: "write",
      // Write-class (not process.exec): `auto_edit` dispatches it without
      // an approval, so the cancel lands on a genuinely live effect.
      capability: "fs.write",
      inputSchema: { type: "object" },
    },
    async () => {
      await slowGate;
      return { data: "late" };
    },
  );
  try {
    await service.createSession({
      sessionId: "pack-sess",
      profileId: "p1",
      model: "test-model",
      executionMode: "auto_edit",
    });

    // Turn 1: approval wait → answer → resume → terminal receipt.
    const sent = await service.send("pack-sess", { content: "list files" });
    assert(sent.kind === "turn", "send() returned kind " + String(sent.kind));
    const waiting = await waitFor(
      () => service.getState("pack-sess"),
      (state) => state.waiting && typeof state.waiting.inputId === "string",
    );
    assert(
      commandEffects === 0,
      "auto_edit dispatched a process.exec callback without approval",
    );
    const ask = waiting.waiting;
    assert(ask.content && ask.content.argsSha256, "ask carries no argument id");
    assert(ask.content.toolName === "run_cmd", "ask is for the wrong tool");
    await service.approve(ask.inputId, "ans-1");
    const turn1 = await pollToDone(service, sent.turnId);
    assert(turn1.done, "the approved turn never settled");
    assert(
      turn1.statuses.includes("completed"),
      "approved turn did not reach completed: " + turn1.statuses.join(","),
    );
    assert(commandEffects === 1, "approved command did not act exactly once");
    assert(
      JSON.stringify(commandInput) === JSON.stringify({ command: "ls -la" }),
      "callback received " + JSON.stringify(commandInput),
    );

    // Turn 2: cancel a live turn mid-effect.
    const sent2 = await service.send("pack-sess", { content: "sleep please" });
    assert(sent2.kind === "turn", "second send() returned a non-turn");
    await waitFor(
      async () => service.pendingCallbacks,
      (count) => count >= 1,
    );
    await service.cancel("pack-sess", sent2.turnId);
    const turn2 = await pollToDone(service, sent2.turnId);
    assert(turn2.done, "the cancelled turn never settled");
    assert(
      !turn2.statuses.includes("completed"),
      "a cancelled turn settled as completed",
    );
    releaseSlow();
    await waitFor(
      async () => service.pendingCallbacks,
      (count) => count === 0,
    );

    const exported = JSON.stringify(await service.exportSession("pack-sess"));
    assert(
      exported.includes("approved and done"),
      "the approved provider reply never reached the session export",
    );
    console.log(
      "packed-native-consumer-ok mode=run runtime=" +
        runtime +
        " events=" +
        (turn1.events.length + turn2.events.length),
    );
  } finally {
    await service.close();
    await store.close();
  }
} else if (mode === "reopen") {
  // Fresh process over the same durable database, with NO provider and NO
  // tools registered: everything must read back from disk, and nothing may
  // re-dispatch a callback.
  const store = await NativeStore.open(dbPath);
  const service = await store.createService();
  try {
    const state = await service.getState("pack-sess");
    assert(state.sessionId === "pack-sess", "session did not reopen");
    const exported = JSON.stringify(await service.exportSession("pack-sess"));
    assert(
      exported.includes("approved and done"),
      "approved turn missing from durable replay",
    );
    assert(
      service.pendingCallbacks === 0,
      "replay re-dispatched a callback in a tool-less process",
    );
    console.log("packed-native-consumer-ok mode=reopen runtime=" + runtime);
  } finally {
    await service.close();
    await store.close();
  }
} else {
  console.error("usage: consumer.mjs <run|reopen> <dbPath>");
  process.exitCode = 2;
}
