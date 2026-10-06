import { describe, expect, test } from "bun:test";
import { parseSseStream } from "../src/index.js";

describe("SSE parser bounds and cleanup", () => {
  test("rejects an oversized unfinished frame and cancels its reader", async () => {
    let cancelled = false;
    const body = new ReadableStream<Uint8Array>({
      start(controller) {
        controller.enqueue(
          new TextEncoder().encode(`data: ${"x".repeat(100)}`),
        );
      },
      cancel() {
        cancelled = true;
      },
    });
    const read = async () => {
      for await (const _frame of parseSseStream(body, { maxFrameChars: 32 })) {
      }
    };
    await expect(read()).rejects.toThrow("SSE frame exceeds maxFrameChars");
    expect(cancelled).toBe(true);
    expect(body.locked).toBe(false);
  });

  test("abort cancels a stalled reader even when fetch ignores its signal", async () => {
    let cancelled = false;
    const body = new ReadableStream<Uint8Array>({
      cancel() {
        cancelled = true;
      },
    });
    const controller = new AbortController();
    const reading = (async () => {
      for await (const _frame of parseSseStream(body, {
        signal: controller.signal,
      })) {
      }
    })();
    controller.abort(new Error("reader stopped"));
    await expect(reading).rejects.toThrow("reader stopped");
    expect(cancelled).toBe(true);
    expect(body.locked).toBe(false);
  });
});
