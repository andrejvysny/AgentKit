import {
  available,
  missing,
  required,
} from "../../native/tests/helpers/binding.js";
import { it } from "bun:test";
if ((required || process.env.AGENTKIT_BACKEND === "native") && !available)
  throw new Error(`Native binding required; missing ${missing.join(", ")}`);
export const nativeIt =
  available &&
  (required ||
    process.env.AGENTKIT_BACKEND === "native" ||
    process.env.AGENTKIT_SKIP_NATIVE !== "1")
    ? it
    : it.skip;
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
