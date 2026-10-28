import { afterEach, expect, it } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createEventStamper } from "@agentkit/core";
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

async function interrupted(legacy: boolean) {
  const dir = mkdtempSync(join(tmpdir(), "approval-policy-"));
  dirs.push(dir);
  const path = join(dir, "store.db");
  const state: State = { effects: 0 };
  const f = await open(
    path,
    state,
    legacy ? process.env.AGENTKIT_LEGACY_PACKAGE_ROOT! : undefined,
  );
  const runId = await start(f);
  const original = await pending(f);
  await f.service.approve(original.inputId, "original-answer");
  state.crash = "after_effect";
  await expect(drive(f)).rejects.toThrow();
  f.store.close();
  expect(state.effects).toBe(1);
  return { path, state, runId, original };
}

async function proposeAgain(
  f: Awaited<ReturnType<typeof open>>,
): Promise<void> {
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
  await f.service.send("chat", { content: "consider the same operation" });
  await drive(f);
}

for (const legacy of [false, true]) {
  const test =
    legacy && process.env.AGENTKIT_LEGACY_PACKAGE_ROOT === undefined
      ? it.skip
      : it;
  test(`${legacy ? "actual .14" : "current"}: a denied same-ID replay cannot replace the original argument hash`, async () => {
    const { path, state, runId } = await interrupted(legacy);
    let f = await open(path, state);
    await f.store.tasks.transitionTask(runId, ["interrupted"], "queued");
    const claim = (await f.store.tasks.claimNext({
      ownerId: "denied-replay",
      scopesBusy: [],
      now: f.clock.now(),
    }))!;
    const stamp = createEventStamper({
      firstSeq: await f.store.tasks.nextSeq(runId),
      attemptId: claim.attempt.attemptId,
    });
    await f.store.tasks.appendEvents(
      runId,
      [
        stamp({
          type: "run.tool.requested",
          runId,
          timestamp: f.clock.nowIso(),
          data: {
            toolCallId: "original-call",
            toolName: "write",
            argumentsJson: '{"changed":true}',
          },
        }),
        stamp({
          type: "run.tool.running",
          runId,
          timestamp: f.clock.nowIso(),
          data: { toolCallId: "original-call", toolName: "write" },
        }),
        stamp({
          type: "run.tool.failed",
          runId,
          timestamp: f.clock.nowIso(),
          data: {
            toolCallId: "original-call",
            toolName: "write",
            errorCode: "tool_guard_refused",
            errorMessage: "policy denied the changed operation",
          },
        }),
      ],
      { leaseToken: claim.lease.leaseToken },
    );
    await f.store.tasks.interruptTask!(runId, "denied replay ended");
    await f.store.tasks.transitionTask(runId, ["interrupted"], "cancelled");
    f.store.close();
    state.fullAuto = true;
    f = await open(path, state);
    await proposeAgain(f);
    expect(state.effects).toBe(1);
    expect((await pending(f)).toolCallId).toBe("new-call");
    f.store.close();
  });

  test(`${legacy ? "actual .14" : "current"}: denying fresh uncertainty leaves the original effect unresolved`, async () => {
    const { path, state, runId, original } = await interrupted(legacy);
    let f = await open(path, state);
    await resume(f, runId);
    const fresh = await pending(f);
    expect(fresh.inputId).not.toBe(original.inputId);
    await f.service.deny(fresh.inputId, "deny-repeat");
    await drive(f);
    expect(state.effects).toBe(1);
    const denial = (await f.store.conversations.listMessages("chat")).find(
      (message) => message.role === "tool",
    );
    expect(denial?.content).toContain("approval_denied");
    f.store.close();
    state.fullAuto = true;
    f = await open(path, state);
    await proposeAgain(f);
    expect(state.effects).toBe(1);
    expect((await pending(f)).toolCallId).toBe("new-call");
    f.store.close();
  });

  for (const policy of ["toolAvailable", "guardDenied"] as const) {
    test(`${legacy ? "actual .14" : "current"}: an unused approval cannot override ${policy}`, async () => {
      const dir = mkdtempSync(join(tmpdir(), "approval-unused-policy-"));
      dirs.push(dir);
      const path = join(dir, "store.db");
      const state: State = { effects: 0 };
      let f = await open(
        path,
        state,
        legacy ? process.env.AGENTKIT_LEGACY_PACKAGE_ROOT! : undefined,
      );
      const runId = await start(f);
      await f.service.approve((await pending(f)).inputId, "unused-answer");
      f.store.close();
      state.toolAvailable = policy !== "toolAvailable";
      state.guardDenied = policy === "guardDenied";
      f = await open(path, state);
      await drive(f);
      expect(state.effects).toBe(0);
      expect((await f.store.tasks.getTask(runId))?.status).toBe("completed");
      const refusal = (await f.store.conversations.listMessages("chat")).find(
        (message) => message.role === "tool",
      );
      expect(refusal?.content).toContain(
        policy === "toolAvailable" ? "tool_missing" : "tool_guard_refused",
      );
      expect(await f.store.toolExecutions.listByRun(runId)).toHaveLength(0);
      f.store.close();
    });

    test(`${legacy ? "actual .14" : "current"}: ${policy} recovery never erases uncertainty`, async () => {
      const { path, state, runId } = await interrupted(legacy);
      state.toolAvailable = policy !== "toolAvailable";
      state.guardDenied = policy === "guardDenied";
      let f = await open(path, state);
      await resume(f, runId);
      expect(state.effects).toBe(1);
      expect((await f.store.tasks.getTask(runId))?.status).toBe("completed");
      const refusal = (await f.store.conversations.listMessages("chat")).find(
        (message) => message.role === "tool",
      );
      expect(refusal?.content).toContain(
        policy === "toolAvailable" ? "tool_missing" : "tool_guard_refused",
      );
      f.store.close();
      state.toolAvailable = true;
      state.guardDenied = false;
      state.fullAuto = true;
      f = await open(path, state);
      await proposeAgain(f);
      expect(state.effects).toBe(1);
      expect((await pending(f)).toolCallId).toBe("new-call");
      f.store.close();
    });
  }
}
