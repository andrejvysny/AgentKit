import type { AiToolResult } from "@agentkit/contracts";
import type { AiTool } from "../tools/tool.js";

/**
 * A thrown error's own verdict on whether trying again is worth anything.
 *
 * Read off an optional `retryable` property rather than inferred from the error
 * class: only the thrower knows whether the failure was a dead socket or a
 * rejected argument, and defaulting to `false` keeps an unannotated throw from
 * advertising a retry nobody promised.
 */
function retryableOf(err: unknown): boolean {
  return (
    typeof err === "object" &&
    err !== null &&
    "retryable" in err &&
    (err as { retryable: unknown }).retryable === true
  );
}

export type ToolExecutionOutcome =
  | { ok: true; value: AiToolResult<unknown> }
  | { ok: false; error: string; retryable: boolean };

export async function executeToolSafely(
  tool: AiTool<unknown, unknown>,
  ctx: Parameters<AiTool["execute"]>[0],
  input: unknown,
  defaultTimeoutMs?: number,
): Promise<ToolExecutionOutcome> {
  const declared = tool.definition.timeoutMs;
  const timeoutMs = declared && declared > 0 ? declared : defaultTimeoutMs;
  // No timeout at all: unchanged fast path (every local tool hits this).
  if (!timeoutMs || timeoutMs <= 0) {
    try {
      const value = await tool.execute(ctx, input);
      return { ok: true, value };
    } catch (err) {
      return {
        ok: false,
        error: err instanceof Error ? err.message : String(err),
        retryable: retryableOf(err),
      };
    }
  }
  // Deadline: abort via a controller linked to the run-level signal, so either
  // the run cancelling OR the deadline elapsing aborts the tool's fetch — AND
  // race it, because a tool is free to ignore the signal it was handed. Without
  // the race, an unresponsive remote tool parks the run (and the host's lease)
  // for as long as it feels like.
  const controller = new AbortController();
  const parent = ctx.signal;
  const onParentAbort = () => controller.abort(parent?.reason);
  if (parent) {
    if (parent.aborted) controller.abort(parent.reason);
    else parent.addEventListener("abort", onParentAbort, { once: true });
  }
  let timer: ReturnType<typeof setTimeout> | undefined;
  let timedOut = false;
  // An async wrapper so a synchronous throw from execute() becomes a rejection
  // the race can observe; the no-op catch keeps a LATE rejection (the tool
  // settling after the deadline, when nobody is listening) from surfacing as an
  // unhandled one. A late result is dropped: the failure is already reported.
  const running = (async () =>
    tool.execute({ ...ctx, signal: controller.signal }, input))();
  running.catch(() => {});
  try {
    const value = await Promise.race([
      running,
      new Promise<never>((_resolve, reject) => {
        timer = setTimeout(() => {
          timedOut = true;
          controller.abort();
          reject(
            new Error(
              `Tool ${tool.definition.name} timed out after ${timeoutMs}ms`,
            ),
          );
        }, timeoutMs);
      }),
    ]);
    return { ok: true, value };
  } catch (err) {
    return {
      ok: false,
      error: err instanceof Error ? err.message : String(err),
      // A deadline says nothing about the tool's own retry semantics, and the
      // abort error it produced is the loop's, not the tool's.
      retryable: timedOut ? false : retryableOf(err),
    };
  } finally {
    if (timer !== undefined) clearTimeout(timer);
    if (parent) parent.removeEventListener("abort", onParentAbort);
  }
}
