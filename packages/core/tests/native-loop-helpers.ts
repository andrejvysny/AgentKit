import { available, missing } from "../../native/tests/helpers/binding.js";
import { it } from "bun:test";
if (!available)
  throw new Error(`Native binding required; missing ${missing.join(", ")}`);
// The Rust runtime is the only backend: these tests are ordinary tests now.
export const nativeIt = it;
export async function drainRaw(
  handle: import("@agentkit/native").RawRunHandle,
  blockAfterToolRunning = false,
): Promise<void> {
  let next = handle.next();
  for (;;) {
    const item = JSON.parse(await next) as {
      event?: { type: string };
      result?: unknown;
    };
    if (item.result) return;
    next = handle.next();
    if (blockAfterToolRunning && item.event?.type === "run.tool.running")
      Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 100);
  }
}
