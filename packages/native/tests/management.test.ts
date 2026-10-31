import { expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { NativeStore, bindingId } from "../src/index.js";
import { waitFor } from "../src/management.js";
import { available, missing, required } from "./helpers/binding.js";

if (required && !available)
  throw new Error(`native binding missing: ${missing.join(", ")}`);
const nativeTest = available ? test : test.skip;

test("import and binding identity do not load the addon", () => {
  expect(bindingId).toBe("agentkit-native");
});

test("abort only stops waiting and cleans listeners", async () => {
  const controller = new AbortController();
  let started = 0;
  let rejectWork!: (error: Error) => void;
  const pending = waitFor(
    () => {
      started++;
      return new Promise<void>((_, reject) => {
        rejectWork = reject;
      });
    },
    { signal: controller.signal },
  );
  controller.abort(new Error("stop waiting"));
  await expect(pending).rejects.toThrow("stop waiting");
  rejectWork(new Error("late failure"));
  await Promise.resolve();
  expect(started).toBe(1);
  await expect(
    waitFor(
      async () => {
        started++;
      },
      { signal: controller.signal },
    ),
  ).rejects.toThrow("stop waiting");
  expect(started).toBe(1);
  const other = new AbortController();
  let removed = 0;
  const original = other.signal.removeEventListener.bind(other.signal);
  other.signal.removeEventListener = (...args) => {
    removed++;
    return original(...args);
  };
  expect(await waitFor(async () => 42, { signal: other.signal })).toBe(42);
  expect(removed).toBe(1);
});

nativeTest(
  "management methods persist and enforce closed gate",
  async () => {
    const directory = mkdtempSync(join(tmpdir(), "native-management-"));
    const path = join(directory, "state.sqlite");
    try {
      let store = await NativeStore.open(path);
      let service = await store.createService();
      await service.createSession({
        sessionId: "s1",
        profileId: "p1",
        model: "old",
      });
      expect((await service.setExecutionMode("s1", "plan")).mode).toBe("plan");
      expect(await service.listApprovalGrants("s1")).toEqual([]);
      expect(await service.clearQueue("s1")).toBe(0);
      await expect(
        service.cancelQueuedInput("s1", "missing"),
      ).rejects.toMatchObject({ code: "not_found" });
      await expect(
        service.revokeApprovalGrant("s1", "missing"),
      ).rejects.toBeDefined();
      await service.compact("s1");
      await expect(service.fork("s1", "missing")).rejects.toBeDefined();
      await service.setModel("s1", { model: "new" });
      await service.configure("s1", { skills: [] });
      await service.close();
      for (const operation of [
        () => service.setExecutionMode("s1", "ask"),
        () => service.listApprovalGrants("s1"),
        () => service.revokeApprovalGrant("s1", "missing"),
        () => service.compact("s1"),
        () => service.fork("s1", "missing"),
        () => service.setModel("s1", { model: "x" }),
        () => service.cancelQueuedInput("s1", "missing"),
        () => service.clearQueue("s1"),
        () => service.configure("s1", {}),
      ])
        await expect(operation()).rejects.toMatchObject({ code: "closed" });
      expect(() => service.setProvider(null)).toThrow();
      await store.close();
      store = await NativeStore.open(path);
      service = await store.createService();
      const state = await service.getState("s1");
      expect((state.descriptor as { model: string }).model).toBe("new");
      expect((await service.getExecution("s1"))?.mode).toBe("plan");
      await service.close();
      await store.close();
    } finally {
      rmSync(directory, { recursive: true, force: true });
    }
  },
  20_000,
);

nativeTest(
  "manual compaction runs before the next ordinary provider call",
  async () => {
    const store = await NativeStore.inMemory();
    const service = await store.createService();
    const requests: string[] = [];
    service.setProvider(async (request) => {
      const text = JSON.stringify(request.messages);
      requests.push(text);
      return {
        content: text.includes("<conversation>") ? "checkpoint" : "answer",
        finishReason: "stop",
      };
    });
    try {
      await service.createSession({
        sessionId: "compact",
        profileId: "p1",
        model: "test",
        context: {
          windowTokens: 4096,
          reserveTokens: 1024,
          keepRecentTokens: 1,
        },
      });
      const send = async (content: string) => {
        const sent = await service.send("compact", { content });
        if (sent.kind !== "turn") throw new Error("expected turn");
        const subscription = await service.subscribeTurn(sent.turnId, 0);
        while (!(await subscription.poll(64, 250)).done) {}
      };
      await send("first");
      await send("second");
      await service.compact("compact");
      await send("third");
      expect(requests).toHaveLength(4);
      expect(requests[2]).toContain("<conversation>");
      expect(requests[3]).toContain("<summary>\\ncheckpoint");
      expect((await service.exportSession("compact")).summaries).toHaveLength(
        1,
      );
    } finally {
      await service.close();
      await store.close();
    }
  },
  20_000,
);

nativeTest(
  "queued inputs can be cancelled and cleared while a turn is active",
  async () => {
    const store = await NativeStore.inMemory();
    const service = await store.createService();
    let release!: () => void;
    let entered!: () => void;
    const waiting = new Promise<void>((resolve) => {
      release = resolve;
    });
    const providerEntered = new Promise<void>((resolve) => {
      entered = resolve;
    });
    try {
      service.setProvider(async () => {
        entered();
        await waiting;
        return { content: "done", finishReason: "stop" };
      });
      await service.createSession({
        sessionId: "queue",
        profileId: "p1",
        model: "test",
      });
      const sent = await service.send("queue", { content: "first" });
      expect(sent.kind).toBe("turn");
      await providerEntered;
      const first = await service.followUp("queue", {
        content: "queued one",
        inputId: "q1",
      });
      expect(first.kind).toBe("queued");
      const second = await service.followUp("queue", {
        content: "queued two",
        inputId: "q2",
      });
      expect(second.kind).toBe("queued");
      expect(await service.cancelQueuedInput("queue", "q1")).toBe(true);
      expect(await service.cancelQueuedInput("queue", "q1")).toBe(false);
      expect(await service.clearQueue("queue")).toBe(1);
      expect(await service.clearQueue("queue")).toBe(0);
      release();
      if (sent.kind === "turn") {
        const subscription = await service.subscribeTurn(sent.turnId, 0);
        while (!(await subscription.poll(64, 250)).done) {}
        const fork = await service.fork("queue", sent.userMessageId);
        expect(fork.sessionId).not.toBe("queue");
      }
    } finally {
      release();
      await service.close();
      await store.close();
    }
  },
  20_000,
);
