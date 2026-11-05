import { expect, test } from "bun:test";
import { join } from "node:path";
import { available, missing } from "./helpers/binding.js";

if (!available) {
  throw new Error(
    `native callback stress requires binding: ${missing.join(", ")}`,
  );
}

const fixture = join(import.meta.dir, "fixtures", "callback-stress-fixture.ts");

interface StressResult {
  kind: "result";
  batches: number;
  batchSize: number;
  entries: number;
  settled: number;
  duplicates: number;
  cancelled: number;
  closeCalls: number;
  closeMs: number;
  maxPending: number;
}

async function readMessages(
  stdout: ReadableStream<Uint8Array>,
  onHeartbeat: () => void,
): Promise<StressResult | undefined> {
  let buffered = "";
  let result: StressResult | undefined;
  const decoder = new TextDecoder();
  for await (const chunk of stdout) {
    buffered += decoder.decode(chunk, { stream: true });
    let newline = buffered.indexOf("\n");
    while (newline >= 0) {
      const line = buffered.slice(0, newline);
      buffered = buffered.slice(newline + 1);
      const message = JSON.parse(line) as { kind: string };
      if (message.kind === "heartbeat") onHeartbeat();
      else if (message.kind === "result") result = message as StressResult;
      newline = buffered.indexOf("\n");
    }
  }
  return result;
}

/** The watchdog lives outside the process whose synchronous native calls can hang. */
async function watchChild(freeze = false): Promise<StressResult> {
  const child = Bun.spawn(
    [process.execPath, fixture, ...(freeze ? ["--freeze"] : [])],
    {
      stdout: "pipe",
      stderr: "pipe",
    },
  );
  const stderr = new Response(child.stderr).text();
  let heartbeatAt = Date.now();
  let heartbeatCount = 0;
  let failure: string | undefined;
  const started = Date.now();
  const watchdog = setInterval(() => {
    const heartbeatLimit = freeze && heartbeatCount > 0 ? 300 : 10_000;
    if (Date.now() - heartbeatAt > heartbeatLimit) {
      failure =
        heartbeatCount > 0
          ? "child heartbeat froze"
          : "child never started heartbeat";
      child.kill("SIGKILL");
    } else if (Date.now() - started > 180_000) {
      failure = "child exceeded external 180-second watchdog";
      child.kill("SIGKILL");
    }
  }, 50);
  try {
    const result = await readMessages(child.stdout, () => {
      heartbeatAt = Date.now();
      heartbeatCount += 1;
    });
    const exitCode = await child.exited;
    if (failure !== undefined) throw new Error(failure);
    expect(exitCode, await stderr).toBe(0);
    expect(heartbeatCount).toBeGreaterThan(1);
    expect(result).toBeDefined();
    return result!;
  } finally {
    clearInterval(watchdog);
    child.kill("SIGKILL");
    await child.exited;
    await stderr;
  }
}

test("external callback watchdog kills a frozen child event loop", async () => {
  await expect(watchChild(true)).rejects.toThrow("child heartbeat froze");
}, 10_000);

test("native callback bridge sustains 32-way batches, cancellation and concurrent close", async () => {
  const result = await watchChild();
  expect(result.batchSize).toBe(32);
  expect(result.batches).toBeGreaterThanOrEqual(100);
  expect(result.settled).toBeGreaterThanOrEqual(3_200);
  expect(result.entries).toBe(3_264);
  expect(result.duplicates).toBe(0);
  expect(result.cancelled).toBe(32);
  expect(result.closeCalls).toBe(32);
  expect(result.closeMs).toBeLessThan(10_000);
  expect(result.maxPending).toBe(32);
}, 195_000);
