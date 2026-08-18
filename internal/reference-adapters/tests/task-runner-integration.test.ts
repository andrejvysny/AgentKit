/**
 * The pre-e2e smoke: the real {@link TurnRunner} driven by the real
 * {@link SingleProcessTaskRunner} over the real {@link MemoryAssistantStore},
 * with only the provider faked.
 *
 * Everything else in this suite tests the runner against a scripted worker. This
 * one exists because the interesting seams are BETWEEN the pieces: the run row
 * TurnRunner writes in its transaction is the row the runner claims; the lease
 * the runner grants is the token TurnRunner's `appendEvents` carries; the
 * terminal transition TurnRunner performs is the one the runner must notice and
 * not repeat.
 */
import { describe, expect, it } from "bun:test";
import { TurnRunner, defaultIds } from "@agentkit/host";
import { MockProviderClient } from "@agentkit/testing";
import { MemoryAssistantStore, SingleProcessTaskRunner } from "../src/index.js";
import { createTestClock, waitFor } from "./support/task-runner-harness.js";

describe("SingleProcessTaskRunner + TurnRunner", () => {
  it("(l) carries a submitted message through to a completed run", async () => {
    const clock = createTestClock();
    const store = new MemoryAssistantStore({ clock });
    const taskRunner = new SingleProcessTaskRunner({
      store,
      clock,
      pollMs: 5,
      heartbeatMs: 60_000,
    });
    const provider = new MockProviderClient();
    provider.setScript([{ steps: [{ kind: "text", content: "Hello there." }] }]);

    await store.providers.upsertProvider({
      id: "p1",
      label: "Mock",
      kind: "openai-compatible",
      baseUrl: "http://localhost:1234",
      defaultModel: "m1",
      enabled: true,
    });
    await store.settings.updateSettings({ defaultProviderId: "p1" });
    await store.conversations.createChat({ id: "chat-1" });

    const turnRunner = new TurnRunner({
      store,
      taskRunner,
      providerFactory: () => provider,
      contributors: [],
      clock,
      ids: defaultIds,
    });

    const handle = await taskRunner.startWorker(turnRunner, {
      concurrency: 1,
      ownerId: "owner-1",
    });
    try {
      const submitted = await turnRunner.submitMessage({
        chatId: "chat-1",
        content: "Hi",
      });

      await waitFor(
        async () =>
          (await store.runs.getRun(submitted.runId))?.status === "completed",
        "the submitted run to complete",
      );

      // The placeholder the submit transaction wrote is where the answer landed.
      const messages = await store.conversations.listMessages("chat-1");
      const assistant = messages.find(
        (message) => message.id === submitted.assistantMessageId,
      );
      expect(assistant?.content).toBe("Hello there.");
      expect(assistant?.metadata["placeholder"]).toBe(false);
      expect(messages[0]?.role).toBe("user");

      // One attempt, one unbroken event sequence, written under the lease the
      // runner granted.
      const events = await store.runs.listEvents(submitted.runId);
      expect(events.length).toBeGreaterThan(0);
      expect(events.map((event) => event.seq)).toEqual(
        events.map((_, index) => index),
      );
      expect(events[0]?.type).toBe("run.started");
      expect(events.some((event) => event.type === "run.message.completed")).toBe(
        true,
      );

      const attempts = [...store.runs.attempts.values()];
      expect(attempts.length).toBe(1);
      expect(attempts[0]?.status).toBe("completed");
      expect(provider.callCount).toBe(1);
    } finally {
      await handle.stop();
    }
  });
});
