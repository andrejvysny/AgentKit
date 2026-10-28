import { afterEach, expect, it } from "bun:test";
import { existsSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { SingleProcessTaskRunner } from "../../runner-local/src/index.js";
import { open, drive, start, pending } from "./fixtures/approval-fixture.js";
import { recordEffect } from "./fixtures/approval-sentinel.js";

const dirs: string[] = [];
afterEach(() => {
  for (const dir of dirs.splice(0))
    rmSync(dir, { recursive: true, force: true });
});

function sentinelLines(path: string): string[] {
  return existsSync(path)
    ? readFileSync(path, "utf8").trim().split("\n").filter(Boolean)
    : [];
}

async function announced(stdout: ReadableStream<Uint8Array>): Promise<string> {
  const reader = stdout.getReader();
  let content = "";
  try {
    while (!content.includes("\n")) {
      const next = await reader.read();
      if (next.done)
        throw new Error(`Child exited before crash barrier: ${content}`);
      content += new TextDecoder().decode(next.value);
    }
    return content.trim();
  } finally {
    reader.releaseLock();
  }
}

async function killAt(
  database: string,
  sentinel: string,
  barrierFile: string,
  phase: string,
  legacyRoot?: string,
): Promise<number> {
  const child = Bun.spawn(
    [
      process.execPath,
      join(import.meta.dir, "fixtures/approval-sigkill-child.ts"),
      database,
      sentinel,
      barrierFile,
      phase,
    ],
    {
      stdout: "pipe",
      stderr: "pipe",
      env: {
        ...process.env,
        ...(legacyRoot === undefined
          ? {}
          : { AGENTKIT_CHILD_PACKAGE_ROOT: legacyRoot }),
      },
    },
  );
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    const announcement = await Promise.race([
      announced(child.stdout),
      new Promise<never>((_resolve, reject) => {
        timer = setTimeout(
          () => reject(new Error("Child crash barrier timed out.")),
          10_000,
        );
      }),
    ]);
    expect(JSON.parse(announcement)).toMatchObject({ phase, pid: child.pid });
    expect(JSON.parse(readFileSync(barrierFile, "utf8"))).toMatchObject({
      phase,
      pid: child.pid,
    });
    child.kill("SIGKILL");
    await child.exited;
    expect(child.signalCode).toBe("SIGKILL");
    expect(await new Response(child.stderr).text()).toBe("");
    return child.pid;
  } finally {
    if (timer) clearTimeout(timer);
    if (child.exitCode === null) child.kill("SIGKILL");
    await child.exited;
  }
}

for (const scenario of [
  { phase: "running" },
  { phase: "admission" },
  { phase: "effect" },
  { phase: "result" },
  { phase: "effect", legacy: true },
] as const) {
  const { phase } = scenario;
  const legacy = "legacy" in scenario;
  it.skipIf(legacy && process.env.AGENTKIT_LEGACY_PACKAGE_ROOT === undefined)(
    `${legacy ? "actual .14 " : ""}SIGKILL after ${phase}: SQLite restores proof without silently repeating an effect`,
    async () => {
      const dir = mkdtempSync(join(tmpdir(), "approval-sigkill-"));
      dirs.push(dir);
      const database = join(dir, "store.db");
      const sentinel = join(dir, "effects.jsonl");
      const legacyRoot = legacy
        ? process.env.AGENTKIT_LEGACY_PACKAGE_ROOT!
        : undefined;
      let f = await open(database, { effects: 0 }, legacyRoot);
      const runId = await start(f);
      const original = await pending(f);
      await f.service.approve(original.inputId, "allow-once");
      f.store.close();
      const childPid = await killAt(
        database,
        sentinel,
        join(dir, "barrier.json"),
        phase,
        legacyRoot,
      );
      const future = new Date(Date.now() + 60_000);
      const clock = { now: () => future, nowIso: () => future.toISOString() };
      const recoveredState = { effects: 0 };
      const reopen = () =>
        open(database, recoveredState, undefined, {
          clock,
          afterEffect: async (ctx) => recordEffect(sentinel, ctx),
        });
      f = await reopen();
      const receipts = await f.store.toolExecutions.listByRun(runId);
      const receipt = receipts[0];
      if (legacy || phase === "running") expect(receipts).toHaveLength(0);
      else {
        expect(receipt?.status).toBe(
          phase === "result" ? "completed" : "admitted",
        );
        expect(receipt?.approvalRequestId).toBe(original.inputId);
      }
      expect(
        (await f.store.tasks.listEvents(runId)).some(
          (event) => event.type === "run.tool.succeeded",
        ),
      ).toBe(false);
      const effects = sentinelLines(sentinel);
      expect(effects).toHaveLength(
        phase === "admission" || phase === "running" ? 0 : 1,
      );
      if (effects.length)
        expect(JSON.parse(effects[0]!)).toMatchObject({
          pid: childPid,
          ...(legacy ? {} : { executionId: receipt!.executionId }),
          runId,
        });
      const root = process.env.AGENTKIT_TEST_PACKAGE_ROOT;
      const local =
        root === undefined
          ? { SingleProcessTaskRunner }
          : ((await import(
              `${root}/dist/runner-local/index.js`
            )) as typeof import("../../runner-local/src/index.js"));
      const recovery = new local.SingleProcessTaskRunner({
        store: f.store,
        clock,
        recoveryMode: "manual",
      });
      expect((await recovery.recoverWithReport()).expired).toBe(1);
      expect((await f.store.tasks.getTask(runId))?.status).toBe("interrupted");
      await f.store.tasks.transitionTask(runId, ["interrupted"], "queued");
      await Promise.all([drive(f), drive(f)]);
      if (phase === "result" || phase === "running") {
        expect((await f.store.tasks.getTask(runId))?.status).toBe("completed");
        expect(
          (await f.store.conversations.listMessages("chat")).find(
            (message) => message.role === "tool",
          )?.content,
        ).toContain('"effects":1');
      } else {
        const fresh = await pending(f);
        expect(fresh.inputId).not.toBe(original.inputId);
        const generation = (fresh.content as { approvalGeneration: string })
          .approvalGeneration;
        if (legacy) expect(generation).toHaveLength(64);
        else expect(generation).toBe(receipt!.executionId);
        expect(f.mock.callCount).toBe(0);
        f.store.close();
        f = await reopen();
        expect((await pending(f)).inputId).toBe(fresh.inputId);
        for (let repeat = 0; repeat < 2; repeat += 1) {
          await f.store.tasks.transitionTask(
            runId,
            ["waiting_approval"],
            "queued",
          );
          await drive(f);
          expect((await pending(f)).inputId).toBe(fresh.inputId);
        }
        const denial = await f.service.deny(fresh.inputId, "deny-repeat");
        const denied = await f.store.conversations.getMessage(denial.messageId);
        expect(String(denied?.content)).toContain(
          '"priorExecutionOutcome":"unknown"',
        );
        expect(String(denied?.content)).toContain(
          "This retry was not executed.",
        );
        await drive(f);
        expect((await f.store.tasks.getTask(runId))?.status).toBe("completed");
        expect(await f.store.toolExecutions.listByRun(runId)).toHaveLength(
          legacy ? 0 : 1,
        );
      }
      if (phase === "running") {
        expect(sentinelLines(sentinel)).toHaveLength(1);
        expect(recoveredState.effects).toBe(1);
        expect(
          (await f.store.toolExecutions.listByRun(runId))[0]?.approvalRequestId,
        ).toBe(original.inputId);
      } else {
        expect(sentinelLines(sentinel)).toEqual(effects);
        expect(recoveredState.effects).toBe(0);
      }
      f.store.close();
    },
    15_000,
  );
}
