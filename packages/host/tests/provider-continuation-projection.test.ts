import { describe, expect, it } from "bun:test";
import { createEventStamper } from "@agentkit/core";
import { createRunProjector, type AssistantStore } from "../src/index.js";
import { fixture } from "./provider-continuation-helpers.js";

async function setup() {
  const environment = await fixture();
  const submitted = await environment.runner.submitMessage({
    chatId: environment.chatId,
    content: "Find",
  });
  const claim = await environment.store.tasks.claimNext({
    ownerId: "test",
    now: environment.clock.now(),
    scopesBusy: [],
  });
  if (!claim) throw new Error("Missing claim");
  const ctx = {
    task: claim.task,
    attemptId: claim.attempt.attemptId,
    leaseToken: claim.lease.leaseToken,
  };
  const stamp = createEventStamper();
  const completed = stamp({
    type: "run.message.completed",
    runId: submitted.runId,
    timestamp: environment.clock.nowIso(),
    data: {
      content: "Checking",
      toolCallCount: 1,
      toolCalls: [{ id: "call-one", name: "lookup", argumentsJson: "{}" }],
    },
  });
  return { ...environment, submitted, ctx, stamp, completed };
}

describe("Responses canonical projection recovery", () => {
  it("completed durable events replay canonical records with stable IDs once", async () => {
    const environment = await setup();
    const { store, ctx, submitted, completed, clock } = environment;
    await store.tasks.appendEvents(submitted.runId, [completed], {
      leaseToken: ctx.leaseToken,
    });
    const projector = createRunProjector({ store, clock });
    const input = {
      chatId: environment.chatId,
      assistantMessageId: submitted.assistantMessageId,
      preserveCanonicalTurns: true,
    };
    await projector.replay(ctx, projector.createState(input));
    await projector.replay(ctx, projector.createState(input));
    const records = await store.conversations.listMessages(environment.chatId);
    const canonical = records.filter(
      (record) => record.metadata.canonicalProviderTurn,
    );
    expect(canonical).toHaveLength(1);
    expect(canonical[0]?.id).toBe(
      `run-event:${submitted.runId}:${completed.eventId}`,
    );
    expect(canonical[0]?.content).toBe("Checking");
    expect(canonical[0]?.toolCalls).toEqual([
      { id: "call-one", name: "lookup", argumentsJson: "{}" },
    ]);
    expect(
      (await store.conversations.getMessage(submitted.assistantMessageId))
        ?.content,
    ).toBe("");
    expect(environment.sent).toHaveLength(0);
  });

  it("rollback deletes newly added optional projection state before replay", async () => {
    const environment = await setup();
    const { store, ctx, submitted, clock } = environment;
    let fail = true;
    const guarded = new Proxy(store, {
      get(target, property) {
        if (property === "transaction")
          return async <T>(run: (tx: AssistantStore) => Promise<T>) =>
            target.transaction(async (tx) => {
              const result = await run(tx);
              if (fail) {
                fail = false;
                throw new Error("Projection rollback");
              }
              return result;
            });
        const value = Reflect.get(target, property) as unknown;
        return typeof value === "function" ? value.bind(target) : value;
      },
    }) as AssistantStore;
    const projector = createRunProjector({ store: guarded, clock });
    const state = projector.createState({
      chatId: environment.chatId,
      assistantMessageId: submitted.assistantMessageId,
    });
    const event = environment.stamp({
      type: "run.message.completed",
      runId: submitted.runId,
      timestamp: clock.nowIso(),
      data: { content: "Partial call", toolCallCount: 1 },
    });
    await expect(projector.project(ctx, state, event)).rejects.toThrow(
      "Projection rollback",
    );
    expect(Object.hasOwn(state, "pendingAssistantMessageId")).toBe(false);
    expect(state.pendingToolCalls).toEqual([]);
    expect(state.projectedSeq).toBe(-1);
    await projector.replay(ctx, state);
    expect(state.pendingAssistantMessageId).toBe(
      `run-event:${submitted.runId}:${event.eventId}`,
    );
    expect(
      (await store.conversations.listMessages(environment.chatId)).filter(
        (record) => record.metadata.internal,
      ),
    ).toHaveLength(1);
  });
});
