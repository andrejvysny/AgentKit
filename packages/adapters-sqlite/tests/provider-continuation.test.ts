import { afterEach, describe, expect, it } from "bun:test";
import type { AiProviderContinuation } from "@agentkit/contracts";
import { MemoryAssistantStore } from "@agentkit/adapters-memory";
import { LeaseLostError, type Clock } from "@agentkit/host";
import { SqliteAssistantStore } from "../src/index.js";
import { createRestHandler } from "../../transport-http/src/index.js";

const closed: (() => void)[] = [];
afterEach(() => {
  for (const close of closed.splice(0)) close();
});

async function fixture(adapter: "memory" | "sqlite", scopeId = "chat-1") {
  let now = Date.parse("2026-10-06T10:00:00Z");
  const clock: Clock = {
    now: () => new Date(now),
    nowIso: () => new Date(now).toISOString(),
  };
  const store =
    adapter === "memory"
      ? new MemoryAssistantStore({ clock })
      : new SqliteAssistantStore(":memory:", { clock });
  if (store instanceof SqliteAssistantStore) closed.push(() => store.close());
  await store.conversations.createChat({ id: "chat-1" });
  await store.tasks.createTask({
    taskId: "run-1",
    kind: "chat.turn",
    scopeId,
    payload: { chatId: "chat-1" },
  });
  const claimed = (await store.tasks.claimNext({
    ownerId: "worker",
    now: clock.now(),
    scopesBusy: [],
  }))!;
  const anchor = await store.conversations.appendMessage({
    chatId: "chat-1",
    runId: "run-1",
    role: "assistant",
    content: "visible answer",
  });
  const state: AiProviderContinuation = {
    version: 1,
    scope: {
      providerId: "provider-1",
      protocol: "responses",
      connectionId: "connection-1",
      generation: 0,
      chatId: "chat-1",
      branchId: "branch-1",
      model: "model-1",
    },
    messageCount: 1,
    messageDigest: "f".repeat(64),
    inputItems: [
      { type: "reasoning", encrypted_content: "private-provider-ciphertext" },
    ],
  };
  const fence = { taskId: "run-1", leaseToken: claimed.lease.leaseToken };
  return {
    store,
    state,
    fence,
    anchor,
    expire: () => {
      now += 30_001;
    },
  };
}

for (const adapter of ["memory", "sqlite"] as const) {
  describe(`${adapter} private continuation store`, () => {
    it("allows a bound run before success and detects loss after a successful save", async () => {
      const { store, state, fence, anchor } = await fixture(adapter);
      await store.continuations.bindRun("run-1", state.scope, fence);
      expect(await store.continuations.getByRun("run-1")).toBeNull();
      await store.continuations.put(
        { runId: "run-1", anchorMessageId: anchor.id, state },
        fence,
      );
      await store.conversations.deleteChat("chat-1");
      expect(await store.continuations.getRunScope("run-1")).toEqual(
        state.scope,
      );
      await expect(store.continuations.getByRun("run-1")).rejects.toMatchObject(
        {
          code: "provider_continuation_state_missing",
        },
      );
    });
    it("supports a shared design serialization scope while enforcing task chat ownership", async () => {
      const { store, state, fence, anchor } = await fixture(
        adapter,
        "shared-design",
      );
      await store.continuations.bindRun("run-1", state.scope, fence);
      await store.continuations.put(
        { runId: "run-1", anchorMessageId: anchor.id, state },
        fence,
      );
      expect(
        (await store.continuations.getByRun("run-1"))?.state.scope.chatId,
      ).toBe("chat-1");
    });
    it("binds immutable run identity and stores one latest defensive snapshot", async () => {
      const { store, state, fence, anchor } = await fixture(adapter);
      await store.continuations.bindRun("run-1", state.scope, fence);
      await store.continuations.bindRun("run-1", { ...state.scope }, fence);
      const input = { runId: "run-1", anchorMessageId: anchor.id, state };
      const saved = await store.continuations.put(input, fence);
      saved.state.inputItems[0]!["encrypted_content"] = "mutated";
      state.inputItems[0]!["encrypted_content"] = "caller mutation";
      expect(
        (await store.continuations.getByRun("run-1"))?.state.inputItems[0]?.[
          "encrypted_content"
        ],
      ).toBe("private-provider-ciphertext");
      const replacement = { ...state, messageCount: 2 };
      await store.continuations.put({ ...input, state: replacement }, fence);
      expect(
        await store.continuations.findByAnchors("chat-1", [
          anchor.id,
          anchor.id,
        ]),
      ).toHaveLength(1);
      expect(
        (await store.continuations.getByRun("run-1"))?.state.messageCount,
      ).toBe(2);
      expect(
        await store.continuations.findByAnchors("other-chat", [anchor.id]),
      ).toEqual([]);
    });

    it("rejects provider, model, account generation and branch rebinding", async () => {
      const { store, state, fence, anchor } = await fixture(adapter);
      await store.continuations.bindRun("run-1", state.scope, fence);
      for (const changed of [
        { providerId: "other" },
        { model: "other" },
        { connectionId: "other" },
        { generation: 1 },
        { branchId: "other" },
        { chatId: "other" },
      ]) {
        const scope = { ...state.scope, ...changed };
        await expect(
          store.continuations.bindRun("run-1", scope, fence),
        ).rejects.toMatchObject({
          code: "provider_continuation_scope_mismatch",
        });
        await expect(
          store.continuations.put(
            {
              runId: "run-1",
              anchorMessageId: anchor.id,
              state: { ...state, scope },
            },
            fence,
          ),
        ).rejects.toMatchObject({
          code: "provider_continuation_scope_mismatch",
        });
      }
      expect(await store.continuations.getRunScope("run-1")).toEqual(
        state.scope,
      );
      expect(await store.continuations.getByRun("run-1")).toBeNull();
    });

    it("looks up only supplied anchors in their active-path order, including 1024 anchors", async () => {
      const { store, state, fence, anchor } = await fixture(adapter);
      await store.continuations.bindRun("run-1", state.scope, fence);
      await store.continuations.put(
        { runId: "run-1", anchorMessageId: anchor.id, state },
        fence,
      );
      await store.tasks.createTask({
        taskId: "run-2",
        kind: "chat.turn",
        scopeId: "chat-1",
        payload: { chatId: "chat-1" },
      });
      const claim = (await store.tasks.claimNext({
        ownerId: "second",
        now: new Date("2026-10-06T10:00:00Z"),
        scopesBusy: [],
      }))!;
      const secondFence = {
        taskId: "run-2",
        leaseToken: claim.lease.leaseToken,
      };
      const secondAnchor = await store.conversations.appendMessage({
        chatId: "chat-1",
        runId: "run-2",
        role: "assistant",
        content: "second",
      });
      await store.continuations.bindRun("run-2", state.scope, secondFence);
      await store.continuations.put(
        { runId: "run-2", anchorMessageId: secondAnchor.id, state },
        secondFence,
      );
      expect(
        (
          await store.continuations.findByAnchors("chat-1", [
            secondAnchor.id,
            anchor.id,
          ])
        ).map((record) => record.runId),
      ).toEqual(["run-2", "run-1"]);
      const many = Array.from(
        { length: 1023 },
        (_value, index) => `missing-${index}`,
      );
      expect(
        (
          await store.continuations.findByAnchors("chat-1", [
            ...many,
            anchor.id,
          ])
        ).map((record) => record.runId),
      ).toEqual(["run-1"]);
    });

    it("refuses unbound state, a cross-chat anchor, and a different fenced run", async () => {
      const { store, state, fence, anchor } = await fixture(adapter);
      const input = { runId: "run-1", anchorMessageId: anchor.id, state };
      await expect(store.continuations.put(input, fence)).rejects.toMatchObject(
        { code: "provider_continuation_scope_mismatch" },
      );
      await store.continuations.bindRun("run-1", state.scope, fence);
      await store.conversations.createChat({ id: "chat-2" });
      const foreign = await store.conversations.appendMessage({
        chatId: "chat-2",
        role: "user",
        content: "foreign",
      });
      await expect(
        store.continuations.put(
          { ...input, anchorMessageId: foreign.id },
          fence,
        ),
      ).rejects.toMatchObject({ code: "provider_continuation_scope_mismatch" });
      await expect(
        store.continuations.put({ ...input, runId: "run-2" }, fence),
      ).rejects.toBeInstanceOf(LeaseLostError);
      const otherRunAnchor = await store.conversations.appendMessage({
        chatId: "chat-1",
        runId: "other-run",
        role: "assistant",
        content: "Foreign run answer",
      });
      await expect(
        store.continuations.put(
          { ...input, anchorMessageId: otherRunAnchor.id },
          fence,
        ),
      ).rejects.toMatchObject({ code: "provider_continuation_scope_mismatch" });
      expect(await store.continuations.getByRun("run-1")).toBeNull();
    });

    it("refuses expired leases without a sweep and preserves the stored state", async () => {
      const { store, state, fence, anchor, expire } = await fixture(adapter);
      await store.continuations.bindRun("run-1", state.scope, fence);
      await store.continuations.put(
        { runId: "run-1", anchorMessageId: anchor.id, state },
        fence,
      );
      expire();
      await expect(
        store.continuations.bindRun("run-1", state.scope, fence),
      ).rejects.toBeInstanceOf(LeaseLostError);
      await expect(
        store.continuations.put(
          {
            runId: "run-1",
            anchorMessageId: anchor.id,
            state: { ...state, messageCount: 2 },
          },
          fence,
        ),
      ).rejects.toBeInstanceOf(LeaseLostError);
      expect(
        (await store.continuations.getByRun("run-1"))?.state.messageCount,
      ).toBe(1);
    });

    it("rejects malformed state, unsafe counts, UTF-8 overflow and excessive anchors", async () => {
      const { store, state, fence, anchor } = await fixture(adapter);
      await store.continuations.bindRun("run-1", state.scope, fence);
      for (const malformed of [
        { ...state, messageDigest: "bad" },
        { ...state, messageCount: -1 },
        { ...state, messageCount: Number.MAX_SAFE_INTEGER + 1 },
        {
          ...state,
          scope: { ...state.scope, generation: Number.MAX_SAFE_INTEGER + 1 },
        },
        { ...state, inputItems: Array.from({ length: 1025 }, () => ({})) },
        { ...state, inputItems: [{ encrypted_content: "😀".repeat(300_000) }] },
      ])
        await expect(
          store.continuations.put(
            { runId: "run-1", anchorMessageId: anchor.id, state: malformed },
            fence,
          ),
        ).rejects.toMatchObject({ code: "invalid_provider_continuation" });
      await expect(
        store.continuations.bindRun(
          "run-1",
          { ...state.scope, generation: -1 },
          fence,
        ),
      ).rejects.toMatchObject({ code: "invalid_provider_continuation" });
      await expect(
        store.continuations.findByAnchors(
          "chat-1",
          Array.from({ length: 1025 }, () => anchor.id),
        ),
      ).rejects.toMatchObject({ code: "invalid_provider_continuation" });
      expect(await store.continuations.getByRun("run-1")).toBeNull();
    });

    it("participates in owner transactions, blocks root reads, and follows task deletion", async () => {
      const { store, state, fence, anchor } = await fixture(adapter);
      let release!: () => void;
      let entered!: () => void;
      const held = new Promise<void>((resolve) => {
        release = resolve;
      });
      const written = new Promise<void>((resolve) => {
        entered = resolve;
      });
      const transaction = store.transaction(async (tx) => {
        await tx.continuations!.bindRun("run-1", state.scope, fence);
        await tx.continuations!.put(
          { runId: "run-1", anchorMessageId: anchor.id, state },
          fence,
        );
        expect((await tx.continuations!.getByRun("run-1"))?.state).toEqual(
          state,
        );
        entered();
        await held;
      });
      await written;
      let observed = false;
      const read = store.continuations.getByRun("run-1").then((record) => {
        observed = true;
        return record;
      });
      await Promise.resolve();
      expect(observed).toBe(false);
      release();
      await transaction;
      expect((await read)?.state).toEqual(state);
      await store.tasks.transitionTask(
        "run-1",
        ["running"],
        "failed",
        {},
        { leaseToken: fence.leaseToken },
      );
      await store.tasks.deleteByScope("chat-1");
      expect(await store.continuations.getByRun("run-1")).toBeNull();
      expect(await store.continuations.getRunScope("run-1")).toBeNull();
    });

    it("keeps private continuation bytes out of messages, task payload and events", async () => {
      const { store, state, fence, anchor } = await fixture(adapter);
      await store.continuations.bindRun("run-1", state.scope, fence);
      await store.continuations.put(
        { runId: "run-1", anchorMessageId: anchor.id, state },
        fence,
      );
      const publicRecords = JSON.stringify({
        messages: await store.conversations.listMessages("chat-1"),
        task: await store.tasks.getTask("run-1"),
        events: await store.tasks.listEvents("run-1"),
      });
      expect(publicRecords).not.toContain("private-provider-ciphertext");
      expect(publicRecords).not.toContain("provider_run_scopes");
      const unavailable = async (): Promise<never> => {
        throw new Error("Not used by reads");
      };
      const handler = createRestHandler({
        store,
        turns: { submitMessage: unavailable, regenerate: unavailable },
        tasks: { cancelTask: unavailable },
      });
      for (const path of ["/v1/chats/chat-1/messages", "/v1/runs/run-1"]) {
        const response = await handler(new Request(`http://localhost${path}`));
        expect(response.status).toBe(200);
        const body = await response.text();
        expect(body).not.toContain("private-provider-ciphertext");
        expect(body).not.toContain("connection-1");
        expect(body).not.toContain("inputItems");
      }
      expect(
        (
          await handler(
            new Request("http://localhost/v1/chats/chat-1/continuations"),
          )
        ).status,
      ).toBe(404);
    });
  });
}
