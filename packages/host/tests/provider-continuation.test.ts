import { describe, expect, it } from "bun:test";
import type { AiRunEvent } from "@agentkit/contracts";
import {
  fixture,
  submitRun,
  execute,
  response,
  reasoning,
  outputText,
  toolCall,
} from "./provider-continuation-helpers.js";
import { SqliteAssistantStore } from "../../adapters-sqlite/src/index.js";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createTestClock } from "./fakes.js";
import { TurnRunner, type AssistantStore } from "../src/index.js";

describe("host Responses continuation", () => {
  it("canonical nonempty tool text replays once; private state never leaks", async () => {
    const environment = await fixture();
    const first = await submitRun(environment);
    expect((await environment.store.tasks.getTask(first.runId))?.status).toBe(
      "completed",
    );
    expect(environment.executed()).toBe(1);
    const messages = await environment.store.conversations.listMessages(
      environment.chatId,
    );
    const visible = messages.find(
      (message) => message.id === first.assistantMessageId,
    );
    expect(visible?.content).toBe("Done");
    expect(visible?.metadata.canonicalProviderDisplay).toBe(true);
    expect(
      messages
        .filter((message) => message.metadata.canonicalProviderTurn)
        .map((message) => message.content),
    ).toEqual(["Checking", "Done"]);
    const stored = await environment.store.continuations?.getByRun(first.runId);
    expect(stored?.state.inputItems).toContainEqual(reasoning);
    const second = await submitRun(environment, "Continue");
    expect((await environment.store.tasks.getTask(second.runId))?.status).toBe(
      "completed",
    );
    expect(environment.sent).toHaveLength(3);
    const body = environment.sent[2]?.body?.input as Record<string, unknown>[];
    expect(body.filter((item) => item.type === "reasoning")).toHaveLength(1);
    expect(body.filter((item) => item.type === "function_call")).toHaveLength(
      1,
    );
    expect(
      body.filter((item) => item.type === "function_call_output"),
    ).toHaveLength(1);
    const publicData = [
      await environment.store.conversations.listMessages(environment.chatId),
      await environment.store.tasks.listEvents(first.runId),
      await environment.store.tasks.getTask(first.runId),
      await environment.store.providers.getProvider("provider"),
    ];
    expect(JSON.stringify(publicData)).not.toContain("private-encrypted-state");
    expect(JSON.stringify(publicData)).not.toContain("inputItems");
  });

  it("SQLite close/reopen preserves reasoning, exact tool IDs and slim outcomes", async () => {
    const directory = mkdtempSync(join(tmpdir(), "agentkit-responses-host-"));
    const path = join(directory, "state.sqlite");
    const clock = createTestClock();
    let store = new SqliteAssistantStore(path, { clock });
    try {
      const environment = await fixture({ store });
      const first = await submitRun(environment);
      expect((await store.tasks.getTask(first.runId))?.status).toBe(
        "completed",
      );
      store.close();
      store = new SqliteAssistantStore(path, { clock });
      const resumed = await fixture({
        store,
        existingChat: environment.chatId,
        output: () => [response([outputText("After reopen")])],
      });
      // Reopened worker IDs must be fresh, as a real host's UUID source is.
      const submitted = await resumed.runner.submitMessage({
        chatId: resumed.chatId,
        content: "Continue",
        taskId: "after-reopen",
      });
      await execute(resumed.runner, store, submitted.runId);
      expect((await store.tasks.getTask(submitted.runId))?.status).toBe(
        "completed",
      );
      const input = resumed.sent[0]?.body?.input as Record<string, unknown>[];
      expect(input).toContainEqual(reasoning);
      expect(input.find((item) => item.type === "function_call")?.call_id).toBe(
        "call-one",
      );
      const result = input.find((item) => item.type === "function_call_output");
      expect(result?.call_id).toBe("call-one");
      expect(String(result?.output)).toContain('"value":"slim"');
      expect(String(result?.output)).not.toContain('"value":"found"');
    } finally {
      store.close();
      rmSync(directory, { recursive: true, force: true });
    }
  });

  it("account generation/model changes reject ancestor state before transport", async () => {
    for (const mismatch of ["generation", "model"] as const) {
      const environment = await fixture();
      await submitRun(environment);
      const before = environment.sent.length;
      if (mismatch === "generation") environment.generation(2);
      const submitted = await environment.runner.submitMessage({
        chatId: environment.chatId,
        content: "Continue",
        ...(mismatch === "model" ? { model: "other" } : {}),
      });
      await expect(
        execute(environment.runner, environment.store, submitted.runId),
      ).rejects.toMatchObject({ code: "provider_continuation_scope_mismatch" });
      expect(environment.sent).toHaveLength(before);
      expect(
        (await environment.store.tasks.getTask(submitted.runId))?.status,
      ).toBe("failed");
    }
  });

  it("forked active branch rejects inherited account state before transport", async () => {
    const environment = await fixture();
    const first = await submitRun(environment);
    const messages = await environment.store.conversations.listMessages(
      environment.chatId,
    );
    const last = messages.at(-1)!;
    await environment.store.conversations.appendMessage({
      chatId: environment.chatId,
      role: "user",
      content: "Sibling",
      parentMessageId: last.id,
    });
    await environment.store.conversations.appendMessage({
      chatId: environment.chatId,
      role: "user",
      content: "New branch",
      parentMessageId: last.id,
    });
    const before = environment.sent.length;
    const submitted = await environment.runner.submitMessage({
      chatId: environment.chatId,
      content: "Continue branch",
    });
    await expect(
      execute(environment.runner, environment.store, submitted.runId),
    ).rejects.toMatchObject({ code: "provider_continuation_scope_mismatch" });
    expect(environment.sent).toHaveLength(before);
    expect(
      await environment.store.continuations?.getByRun(first.runId),
    ).not.toBeNull();
  });

  it("truncated history and changed system prompt fail before provider transport", async () => {
    for (const mismatch of ["history", "prompt"] as const) {
      const environment = await fixture();
      await submitRun(environment);
      const before = environment.sent.length;
      const runner = new TurnRunner({
        ...environment.deps,
        ...(mismatch === "history"
          ? { historyLimit: 2 }
          : {
              context: {
                listBindings: async () => [],
                systemPrompt: async () => "Changed prompt",
              },
            }),
      });
      const submitted = await runner.submitMessage({
        chatId: environment.chatId,
        content: "Continue",
      });
      await execute(runner, environment.store, submitted.runId);
      expect(environment.sent).toHaveLength(before);
      expect(
        (await environment.store.tasks.getTask(submitted.runId))?.status,
      ).toBe("failed");
      expect(
        (
          (await environment.store.tasks.listEvents(
            submitted.runId,
          )) as AiRunEvent[]
        ).find((event) => event.type === "run.failed")?.data,
      ).toMatchObject({ errorCode: "continuation_history_mismatch" });
    }
  });

  it("Responses without private store fails before staging or network", async () => {
    const environment = await fixture();
    const without = new Proxy(environment.store, {
      get(target, property) {
        if (property === "continuations") return undefined;
        const value = Reflect.get(target, property) as unknown;
        return typeof value === "function" ? value.bind(target) : value;
      },
    }) as AssistantStore;
    let staged = false;
    const runner = new TurnRunner({
      ...environment.deps,
      store: without,
      contributors: [
        {
          namespace: "test",
          async contribute() {
            staged = true;
            return [];
          },
        },
      ],
    });
    const submitted = await runner.submitMessage({
      chatId: environment.chatId,
      content: "Find",
    });
    await expect(
      execute(runner, without, submitted.runId),
    ).rejects.toMatchObject({ code: "provider_continuation_store_required" });
    expect(staged).toBe(false);
    expect(environment.sent).toHaveLength(0);
  });

  it("Responses provider error never retries with tools removed", async () => {
    const environment = await fixture({
      output: () => [
        { type: "response.failed", response: { error: { code: "unknown" } } },
      ],
    });
    const submitted = await submitRun(environment);
    expect(environment.sent).toHaveLength(1);
    expect(
      (await environment.store.tasks.getTask(submitted.runId))?.status,
    ).toBe("failed");
  });
  it("correction keeps canonical tool text, original final and corrected final across followup", async () => {
    let verificationCalls = 0;
    const environment = await fixture({
      overrides: {
        correction: { maxPasses: 1 },
        verification: {
          async verify() {
            verificationCalls++;
            return verificationCalls === 1
              ? {
                  status: "partial",
                  checks: [],
                  deficiencies: ["Fix the answer"],
                }
              : { status: "pass", checks: [], deficiencies: [] };
          },
        },
      },
      output: (call) => [
        response(
          call === 1
            ? [reasoning, outputText("Checking", "intro"), toolCall()]
            : [
                outputText(
                  call === 2
                    ? "Original"
                    : call === 3
                      ? "Corrected"
                      : "Followup",
                  `message-${call}`,
                ),
              ],
        ),
      ],
    });
    const first = await submitRun(environment);
    expect((await environment.store.tasks.getTask(first.runId))?.status).toBe(
      "completed",
    );
    expect(
      (
        await environment.store.conversations.getMessage(
          first.assistantMessageId,
        )
      )?.content,
    ).toBe("Corrected");
    const canonical = (
      await environment.store.conversations.listMessages(environment.chatId)
    ).filter((record) => record.metadata.canonicalProviderTurn);
    expect(canonical.map((record) => record.role)).toEqual([
      "assistant",
      "assistant",
      "user",
      "assistant",
    ]);
    expect(
      canonical
        .filter((record) => record.role === "assistant")
        .map((record) => record.content),
    ).toEqual(["Checking", "Original", "Corrected"]);
    const second = await submitRun(environment, "Continue after correction");
    expect((await environment.store.tasks.getTask(second.runId))?.status).toBe(
      "completed",
    );
    expect(environment.sent).toHaveLength(4);
    const input = environment.sent[3]?.body?.input as Record<string, unknown>[];
    expect(input.filter((item) => item.type === "reasoning")).toHaveLength(1);
    expect(
      input.filter((item) => item.type === "function_call_output"),
    ).toHaveLength(1);
    expect(
      input
        .filter((item) => item.role === "assistant")
        .map((item) => (item.content as { text: string }[])[0]?.text),
    ).toEqual(["Checking", "Original", "Corrected"]);
  });

  it("reasoning-only empty retry retains state and explicit followup for the next run", async () => {
    const environment = await fixture({
      output: (call) => [
        response(
          call === 1
            ? [reasoning]
            : [
                outputText(
                  call === 2 ? "Retried" : "Continued",
                  `message-${call}`,
                ),
              ],
        ),
      ],
    });
    const first = await submitRun(environment);
    expect((await environment.store.tasks.getTask(first.runId))?.status).toBe(
      "completed",
    );
    expect(environment.sent).toHaveLength(2);
    expect(environment.sent[1]?.body?.input).toContainEqual(reasoning);
    expect(JSON.stringify(environment.sent[1]?.body?.input)).toContain(
      "Please provide an answer to the previous request.",
    );
    const second = await submitRun(environment, "Continue");
    expect((await environment.store.tasks.getTask(second.runId))?.status).toBe(
      "completed",
    );
    expect(environment.sent).toHaveLength(3);
  });

  it("a restarted bound run cannot change account generation before its first request", async () => {
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
    const records = await environment.store.conversations.listMessages(
      environment.chatId,
    );
    await environment.store.continuations!.bindRun(
      submitted.runId,
      {
        providerId: "provider",
        protocol: "responses",
        connectionId: "account",
        generation: 1,
        chatId: environment.chatId,
        branchId: records[0]!.id,
        model: "model",
      },
      { taskId: submitted.runId, leaseToken: claim.lease.leaseToken },
    );
    environment.generation(2);
    const restarted = new TurnRunner(environment.deps);
    await expect(
      restarted.executeTask({
        task: claim.task,
        attemptId: claim.attempt.attemptId,
        leaseToken: claim.lease.leaseToken,
        signal: new AbortController().signal,
      }),
    ).rejects.toMatchObject({ code: "provider_continuation_scope_mismatch" });
    expect(environment.sent).toHaveLength(0);
  });

  it("state saved before canonical projection never silently drops the missing assistant", async () => {
    const environment = await fixture();
    const port = environment.store.continuations!;
    const put = port.put.bind(port);
    port.put = async (record, fence) => {
      await put(record, fence);
      throw new Error("Crash after private state save");
    };
    const first = await submitRun(environment);
    expect((await environment.store.tasks.getTask(first.runId))?.status).toBe(
      "failed",
    );
    expect(environment.executed()).toBe(0);
    expect(await port.getByRun(first.runId)).not.toBeNull();
    port.put = put;
    const second = await submitRun(environment, "Continue");
    expect(environment.sent).toHaveLength(1);
    expect(
      (
        (await environment.store.tasks.listEvents(second.runId)) as AiRunEvent[]
      ).find((event) => event.type === "run.failed")?.data,
    ).toMatchObject({ errorCode: "continuation_history_mismatch" });
  });
  it("authentication failure before first save permits a later fresh run", async () => {
    const environment = await fixture({
      output: (call) =>
        call === 1
          ? [
              {
                type: "response.failed",
                response: { error: { code: "invalid_api_key" } },
              },
            ]
          : [response([outputText("Recovered")])],
    });
    const first = await submitRun(environment);
    expect((await environment.store.tasks.getTask(first.runId))?.status).toBe(
      "failed",
    );
    expect(
      await environment.store.continuations!.getByRun(first.runId),
    ).toBeNull();
    const second = await submitRun(environment, "Try again");
    expect((await environment.store.tasks.getTask(second.runId))?.status).toBe(
      "completed",
    );
    expect(environment.sent).toHaveLength(2);
  });

  it("a same-identity bound run resumes safely before any state was saved", async () => {
    const environment = await fixture({
      output: () => [response([outputText("Resumed")])],
    });
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
    const records = await environment.store.conversations.listMessages(
      environment.chatId,
    );
    await environment.store.continuations!.bindRun(
      submitted.runId,
      {
        providerId: "provider",
        protocol: "responses",
        connectionId: "account",
        generation: 1,
        chatId: environment.chatId,
        branchId: records[0]!.id,
        model: "model",
      },
      { taskId: submitted.runId, leaseToken: claim.lease.leaseToken },
    );
    await new TurnRunner(environment.deps).executeTask({
      task: claim.task,
      attemptId: claim.attempt.attemptId,
      leaseToken: claim.lease.leaseToken,
      signal: new AbortController().signal,
    });
    expect(
      (await environment.store.tasks.getTask(submitted.runId))?.status,
    ).toBe("completed");
    expect(environment.sent).toHaveLength(1);
  });
});
