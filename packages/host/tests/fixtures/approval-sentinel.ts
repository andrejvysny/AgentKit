import { closeSync, fsyncSync, openSync, writeSync } from "node:fs";
import type { AiToolExecutionContext } from "@agentkit/core";

export function durableWrite(
  path: string,
  content: unknown,
  append = false,
): void {
  const fd = openSync(path, append ? "a" : "w");
  try {
    writeSync(fd, `${JSON.stringify(content)}\n`);
    fsyncSync(fd);
  } finally {
    closeSync(fd);
  }
}

export function recordEffect(path: string, ctx: AiToolExecutionContext): void {
  const executionId = ctx.metadata?.executionAttemptId;
  if (typeof executionId !== "string")
    throw new Error("Missing durable execution identity.");
  durableWrite(
    path,
    {
      executionId,
      runId: ctx.runId,
      toolCallId: ctx.toolCallId,
      pid: process.pid,
    },
    true,
  );
}
