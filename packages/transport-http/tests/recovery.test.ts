import { describe, expect, test } from "bun:test";
import type { ChatDto, RunDto } from "@agentkit/contracts";
import {
  createHandlerFixture,
  request,
  TEST_CHAT_ID,
} from "./support/fixture.js";

describe("run recovery identity", () => {
  test("getChat exposes interrupted identity; resume queues the same run", async () => {
    const resumed: string[] = [];
    const fixture = await createHandlerFixture({
      tasks: {
        cancelTask: async () => {},
        resumeTask: async (runId) => {
          resumed.push(runId);
          await fixture.store.tasks.transitionTask(
            runId,
            ["interrupted"],
            "queued",
          );
        },
      },
    });
    const submitted = await fixture.turnRunner.submitMessage({
      chatId: TEST_CHAT_ID,
      content: "recover",
    });
    await fixture.store.tasks.transitionTask(
      submitted.runId,
      ["queued"],
      "interrupted",
      { error: "manual_recovery" },
    );
    const chat = await fixture.handler(
      request("GET", `/v1/chats/${TEST_CHAT_ID}`),
    );
    expect(((await chat.json()) as ChatDto).activeRunId).toBe(submitted.runId);
    const response = await fixture.handler(
      request("POST", `/v1/runs/${submitted.runId}/resume`),
    );
    expect(response.status).toBe(202);
    const run = (await response.json()) as RunDto;
    expect(run.runId).toBe(submitted.runId);
    expect(run.status).toBe("queued");
    expect(resumed).toEqual([submitted.runId]);
    expect(await fixture.store.tasks.listByScope(TEST_CHAT_ID)).toHaveLength(1);
  });

  test("resume rejects a queued run; missing capability is a typed 501", async () => {
    const fixture = await createHandlerFixture({
      tasks: { cancelTask: async () => {} },
    });
    const submitted = await fixture.turnRunner.submitMessage({
      chatId: TEST_CHAT_ID,
      content: "recover",
    });
    const unsupported = await fixture.handler(
      request("POST", `/v1/runs/${submitted.runId}/resume`),
    );
    expect(unsupported.status).toBe(501);
    expect(await unsupported.json()).toMatchObject({ code: "not_implemented" });
    const wired = await createHandlerFixture({
      tasks: { cancelTask: async () => {}, resumeTask: async () => {} },
    });
    const queued = await wired.turnRunner.submitMessage({
      chatId: TEST_CHAT_ID,
      content: "queued",
    });
    const rejected = await wired.handler(
      request("POST", `/v1/runs/${queued.runId}/resume`),
    );
    expect(rejected.status).toBe(409);
    expect(await rejected.json()).toMatchObject({
      code: "run_not_interrupted",
    });
    expect((await wired.store.tasks.getTask(queued.runId))?.status).toBe(
      "queued",
    );
  });
});
