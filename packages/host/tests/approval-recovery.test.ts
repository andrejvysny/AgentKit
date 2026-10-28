import { afterEach, expect, it } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { Database } from "bun:sqlite";
import {
  open,
  drive,
  start,
  pending,
  resume,
  type State,
} from "./fixtures/approval-fixture.js";

const dirs: string[] = [];
afterEach(() => {
  for (const dir of dirs.splice(0))
    rmSync(dir, { recursive: true, force: true });
});

function dbPath(): string {
  const dir = mkdtempSync(join(tmpdir(), "approval-recovery-"));
  dirs.push(dir);
  return join(dir, "store.db");
}

it("SQLite restart preserves unanswered and answered-unused Allow once", async () => {
  const path = dbPath();
  const state = { effects: 0 };
  let f = await open(path, state);
  const runId = await start(f);
  const original = await pending(f);
  f.store.close();
  f = await open(path, state);
  expect((await pending(f)).inputId).toBe(original.inputId);
  expect(state.effects).toBe(0);
  await f.service.approve(original.inputId, "answer");
  f.store.close();
  f = await open(path, state);
  await Promise.all([drive(f), drive(f)]);
  expect(state.effects).toBe(1);
  expect((await f.store.tasks.getTask(runId))?.status).toBe("completed");
  expect(
    (await f.store.toolExecutions.listByRun(runId))[0]?.approvalRequestId,
  ).toBe(original.inputId);
  f.store.close();
});

for (const crash of ["before_body", "after_effect", "after_result"] as const) {
  it(`SQLite ${crash}: recovery never silently reuses consumed approval`, async () => {
    const path = dbPath();
    const state: State = { effects: 0 };
    let f = await open(path, state);
    const runId = await start(f);
    const original = await pending(f);
    await f.service.approve(original.inputId, "original-answer");
    state.crash = crash;
    await expect(drive(f)).rejects.toThrow();
    expect(state.effects).toBe(crash === "before_body" ? 0 : 1);
    expect((await f.store.tasks.getTask(runId))?.status).toBe("interrupted");
    f.store.close();
    f = await open(path, state);
    await resume(f, runId);
    if (crash === "after_result") {
      expect(state.effects).toBe(1);
      expect((await f.store.tasks.getTask(runId))?.status).toBe("completed");
      const result = (await f.store.conversations.listMessages("chat")).find(
        (record) => record.role === "tool",
      );
      expect(result?.content).toContain('"effects":1');
    } else {
      const fresh = await pending(f);
      expect(fresh.inputId).not.toBe(original.inputId);
      expect(
        (fresh.content as { approvalGeneration?: string }).approvalGeneration,
      ).toHaveLength(64);
      for (let attempt = 0; attempt < 3; attempt += 1) {
        await f.store.tasks.transitionTask(
          runId,
          ["waiting_approval"],
          "queued",
        );
        await drive(f);
        expect((await pending(f)).inputId).toBe(fresh.inputId);
      }
      expect(state.effects).toBe(crash === "before_body" ? 0 : 1);
      f.store.close();
      f = await open(path, state);
      expect((await pending(f)).inputId).toBe(fresh.inputId);
      await Promise.all([
        f.service.approve(fresh.inputId, "fresh-answer"),
        f.service.approve(fresh.inputId, "fresh-answer"),
      ]);
      await Promise.all([drive(f), drive(f)]);
      expect(state.effects).toBe(crash === "before_body" ? 1 : 2);
      const receipts = await f.store.toolExecutions.listByRun(runId);
      expect(receipts).toHaveLength(2);
      expect(receipts[0]?.status).toBe("admitted");
      expect(receipts[1]?.approvalRequestId).toBe(fresh.inputId);
    }
    f.store.close();
  });
}

it("legacy .14 running-without-result stays uncertain after repark/restart", async () => {
  const path = dbPath();
  const state: State = { effects: 0 };
  let f = await open(path, state);
  const runId = await start(f);
  const original = await pending(f);
  await f.service.approve(original.inputId, "original-answer");
  state.crash = "after_effect";
  await expect(drive(f)).rejects.toThrow();
  f.store.close();
  const raw = new Database(path);
  raw.exec("DROP TABLE tool_executions");
  raw.exec(
    "UPDATE task_events SET payload = json_remove(payload, '$.data.admissionProtocol') WHERE type = 'run.tool.running'",
  );
  raw.close();
  f = await open(path, state);
  expect(await f.store.toolExecutions.listByRun(runId)).toEqual([]);
  await resume(f, runId);
  const fresh = await pending(f);
  expect(fresh.inputId).not.toBe(original.inputId);
  f.store.close();
  f = await open(path, state);
  await f.store.tasks.transitionTask(runId, ["waiting_approval"], "queued");
  await drive(f);
  expect((await pending(f)).inputId).toBe(fresh.inputId);
  expect(state.effects).toBe(1);
  await f.service.approve(fresh.inputId, "fresh-answer");
  await drive(f);
  expect(state.effects).toBe(2);
  f.store.close();
});

it("Full auto uncertain mutation still requires fresh authorization", async () => {
  const path = dbPath();
  const state: State = { effects: 0, fullAuto: true, crash: "after_effect" };
  let f = await open(path, state);
  await expect(start(f)).rejects.toThrow();
  const runId = (await f.store.tasks.listByScope("chat"))[0]!.taskId;
  f.store.close();
  f = await open(path, state);
  await resume(f, runId);
  expect((await pending(f)).content).toMatchObject({ kind: "approval" });
  expect(state.effects).toBe(1);
  f.store.close();
});

for (const failure of ["failOnce", "timeoutOnce"] as const) {
  it(`same-turn ${failure}: provider reissue with new ID cannot repeat effect`, async () => {
    const state: State = { effects: 0, fullAuto: true, [failure]: true };
    const f = await open(dbPath(), state);
    f.mock.setScript([
      {
        steps: [
          {
            kind: "tool_call",
            toolCallId: "first",
            name: "write",
            argumentsJson: "{}",
          },
        ],
      },
      {
        steps: [
          {
            kind: "tool_call",
            toolCallId: "new-id",
            name: "write",
            argumentsJson: "{}",
          },
        ],
      },
    ]);
    await f.service.send("chat", { content: "write once" });
    await drive(f);
    expect(state.effects).toBe(1);
    const request = await pending(f);
    expect(request.toolCallId).toBe("new-id");
    expect(
      (request.content as { approvalGeneration: string }).approvalGeneration,
    ).toBe(
      (await f.store.toolExecutions.listByRun(request.targetTaskId!))[0]!
        .executionId,
    );
    f.store.close();
  });
}

it("unreadable recovery history blocks before executing a mutation", async () => {
  const state = { effects: 0 };
  const f = await open(dbPath(), state);
  const runId = await start(f);
  await f.service.approve((await pending(f)).inputId, "answer");
  const listEvents = f.store.tasks.listEvents.bind(f.store.tasks);
  let reads = 0;
  f.store.tasks.listEvents = async (...args) => {
    reads += 1;
    if (reads === 2) throw new Error("unreadable log");
    return listEvents(...args);
  };
  await expect(drive(f)).rejects.toMatchObject({
    code: "recovery_history_unreadable",
  });
  expect(state.effects).toBe(0);
  expect((await f.store.tasks.getTask(runId))?.status).toBe("failed");
  f.store.close();
});

it("two uncertain runs preserve the current new-call approval generation across crashes", async () => {
  const path = dbPath();
  const state: State = { effects: 0, fullAuto: true, crash: "after_effect" };
  let f = await open(path, state);
  await expect(start(f)).rejects.toThrow();
  const oldRun = (await f.store.tasks.listByScope("chat"))[0]!.taskId;
  await f.store.tasks.transitionTask(oldRun, ["interrupted"], "cancelled");
  f.mock.setScript([
    {
      steps: [
        {
          kind: "tool_call",
          toolCallId: "new-call",
          name: "write",
          argumentsJson: "{}",
        },
      ],
    },
  ]);
  const sent = await f.service.send("chat", {
    content: "try after inspection",
  });
  if (sent.kind !== "turn") throw new Error("expected turn");
  await drive(f);
  const firstApproval = await pending(f);
  await f.service.approve(firstApproval.inputId, "first-new-call-answer");
  state.crash = "after_effect";
  await expect(drive(f)).rejects.toThrow();
  expect(state.effects).toBe(2);
  const currentExecution = (
    await f.store.toolExecutions.listByRun(sent.turnId)
  )[0]!;
  f.store.close();
  f = await open(path, state);
  await resume(f, sent.turnId);
  const fresh = await pending(f);
  expect(
    (fresh.content as { approvalGeneration: string }).approvalGeneration,
  ).toBe(currentExecution.executionId);
  expect(fresh.inputId).not.toBe(firstApproval.inputId);
  f.store.close();
  f = await open(path, state);
  await f.service.approve(fresh.inputId, "second-new-call-answer");
  await drive(f);
  expect(state.effects).toBe(3);
  expect((await f.store.tasks.getTask(sent.turnId))?.status).toBe("completed");
  f.store.close();
});
