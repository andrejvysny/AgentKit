import { open, drive } from "./approval-fixture.js";
import { durableWrite, recordEffect } from "./approval-sentinel.js";

const [database, sentinel, barrierFile, phase] = process.argv.slice(2);
if (!database || !sentinel || !barrierFile || !phase)
  throw new Error("Missing child fixture arguments.");

async function barrier(at: string, executionId: string): Promise<void> {
  if (at !== phase) return;
  const evidence = { phase, executionId, pid: process.pid };
  durableWrite(barrierFile!, evidence);
  process.stdout.write(`${JSON.stringify(evidence)}\n`);
  // Keep the process alive until the parent delivers SIGKILL.
  setInterval(() => {}, 60_000);
  await new Promise<void>(() => {});
}

const fixture = await open(
  database,
  { effects: 0 },
  process.env.AGENTKIT_CHILD_PACKAGE_ROOT,
  {
    afterRunning: (id) => barrier("running", id),
    afterAdmission: (id) => barrier("admission", id),
    async afterEffect(ctx) {
      const executionId =
        typeof ctx.metadata?.executionAttemptId === "string"
          ? ctx.metadata.executionAttemptId
          : process.env.AGENTKIT_CHILD_PACKAGE_ROOT
            ? `legacy:${ctx.runId}:${ctx.toolCallId}`
            : undefined;
      if (executionId === undefined)
        throw new Error("Missing durable execution identity.");
      if (process.env.AGENTKIT_CHILD_PACKAGE_ROOT)
        durableWrite(
          sentinel!,
          {
            executionId,
            runId: ctx.runId,
            toolCallId: ctx.toolCallId,
            pid: process.pid,
          },
          true,
        );
      else recordEffect(sentinel!, ctx);
      await barrier("effect", executionId);
    },
    afterResult: (id) => barrier("result", id),
  },
);
await drive(fixture);
throw new Error("Child missed its configured crash barrier.");
