import { describe, expect, it } from "bun:test";
import { LeaseLostError, TurnRunner } from "../src/index.js";
import {
  execute,
  fixture,
  outputText,
  response,
} from "./provider-continuation-helpers.js";

describe("Responses run-anchored continuation", () => {
  it("executes an older queued run without inheriting a later completed turn", async () => {
    const environment = await fixture({
      overrides: { allowConcurrentSubmit: true },
      output: () => [response([outputText("Answer")])],
    });
    const original = await environment.runner.submitMessage({
      chatId: environment.chatId,
      content: "Original question",
    });
    const later = await environment.runner.submitMessage({
      chatId: environment.chatId,
      content: "Future question",
      priority: 10,
    });
    await execute(environment.runner, environment.store, later.runId);
    await execute(environment.runner, environment.store, original.runId);
    expect(
      (await environment.store.tasks.getTask(original.runId))?.status,
    ).toBe("completed");
    expect(environment.sent).toHaveLength(2);
    expect(JSON.stringify(environment.sent[1]?.body?.input)).toContain(
      "Original question",
    );
    expect(JSON.stringify(environment.sent[1]?.body?.input)).not.toContain(
      "Future question",
    );
  });

  it("keeps execution on its own branch after the active path switches away", async () => {
    const environment = await fixture({
      output: () => [response([outputText("Original answer")])],
    });
    const original = await environment.runner.submitMessage({
      chatId: environment.chatId,
      content: "Original question",
    });
    await environment.store.conversations.appendMessage({
      chatId: environment.chatId,
      role: "user",
      content: "Other branch",
      parentMessageId: original.userMessageId,
    });
    await execute(environment.runner, environment.store, original.runId);
    expect(
      (await environment.store.tasks.getTask(original.runId))?.status,
    ).toBe("completed");
    expect(JSON.stringify(environment.sent[0]?.body?.input)).not.toContain(
      "Other branch",
    );
    expect(
      (
        await environment.store.conversations.listMessages(environment.chatId)
      ).at(-1)?.content,
    ).toBe("Other branch");
  });

  it("resumes a bound run under the original identity after a branch switch", async () => {
    const environment = await fixture({
      output: () => [response([outputText("Resumed")])],
    });
    const original = await environment.runner.submitMessage({
      chatId: environment.chatId,
      content: "Original question",
    });
    const claim = await environment.store.tasks.claimNext({
      ownerId: "test",
      now: environment.clock.now(),
      scopesBusy: [],
    });
    if (!claim) throw new Error("Missing claim");
    await environment.store.continuations!.bindRun(
      original.runId,
      {
        providerId: "provider",
        protocol: "responses",
        connectionId: "account",
        generation: 1,
        chatId: environment.chatId,
        branchId: original.userMessageId,
        model: "model",
      },
      { taskId: original.runId, leaseToken: claim.lease.leaseToken },
    );
    await environment.store.conversations.appendMessage({
      chatId: environment.chatId,
      role: "user",
      content: "Switched branch",
      parentMessageId: original.userMessageId,
    });
    await new TurnRunner(environment.deps).executeTask({
      task: claim.task,
      attemptId: claim.attempt.attemptId,
      leaseToken: claim.lease.leaseToken,
      signal: new AbortController().signal,
    });
    expect(
      (await environment.store.tasks.getTask(original.runId))?.status,
    ).toBe("completed");
    expect(environment.sent).toHaveLength(1);
    expect(JSON.stringify(environment.sent[0]?.body?.input)).not.toContain(
      "Switched branch",
    );
  });

  it("resumes durable private state and its completed tool work after a branch switch", async () => {
    const environment = await fixture();
    let leaseToken = "";
    const crashing = new TurnRunner({
      ...environment.deps,
      providerFactory: (config) => {
        const client = environment.deps.providerFactory(config);
        const stream = client.streamChat.bind(client);
        let calls = 0;
        client.streamChat = async function* (input) {
          if (++calls === 2) {
            await environment.store.tasks.releaseLease(leaseToken);
            throw new LeaseLostError("Worker died after tool persistence");
          }
          yield* stream(input);
        };
        return client;
      },
    });
    const original = await crashing.submitMessage({
      chatId: environment.chatId,
      content: "Original question",
    });
    const claim = await environment.store.tasks.claimNext({
      ownerId: "test",
      now: environment.clock.now(),
      scopesBusy: [],
    });
    if (!claim) throw new Error("Missing claim");
    leaseToken = claim.lease.leaseToken;
    await expect(
      crashing.executeTask({
        task: claim.task,
        attemptId: claim.attempt.attemptId,
        leaseToken: claim.lease.leaseToken,
        signal: new AbortController().signal,
      }),
    ).rejects.toBeInstanceOf(LeaseLostError);
    expect(
      (await environment.store.tasks.getTask(original.runId))?.status,
    ).toBe("running");
    expect(
      await environment.store.continuations!.getByRun(original.runId),
    ).not.toBeNull();
    await environment.store.conversations.appendMessage({
      chatId: environment.chatId,
      role: "user",
      content: "Switched branch",
      parentMessageId: original.userMessageId,
    });
    const attempt = await environment.store.tasks.createAttempt({
      taskId: original.runId,
      attemptId: environment.ids.attemptId(),
      ownerId: "restart",
    });
    const lease = await environment.store.tasks.acquireLease({
      taskId: original.runId,
      attemptId: attempt.attemptId,
      ownerId: "restart",
      ttlMs: 60_000,
    });
    const task = await environment.store.tasks.getTask(original.runId);
    if (!task) throw new Error("Missing run");
    await new TurnRunner(environment.deps).executeTask({
      task,
      attemptId: attempt.attemptId,
      leaseToken: lease.leaseToken,
      signal: new AbortController().signal,
    });
    expect(
      (await environment.store.tasks.getTask(original.runId))?.status,
    ).toBe("completed");
    expect(environment.executed()).toBe(1);
    expect(environment.sent).toHaveLength(2);
    expect(JSON.stringify(environment.sent[1]?.body?.input)).not.toContain(
      "Switched branch",
    );
    expect(JSON.stringify(environment.sent[1]?.body?.input)).toContain(
      "private-encrypted-state",
    );
  });
});
