import { describe, expect, it } from "bun:test";
import { MockProviderClient } from "@agentkit/testing";
import { TurnRunner } from "../src/index.js";
import { assembleMessages } from "../src/turn/history-assembly.js";
import { createHarness } from "./fakes.js";

async function fixture(allowConcurrentSubmit = true) {
  const harness = createHarness();
  const chat = await harness.store.conversations.createChat({});
  const root = await harness.store.conversations.appendMessage({
    chatId: chat.id,
    role: "user",
    content: "Old root",
  });
  const deps = {
    ...harness,
    contributors: [],
    providerFactory: () => new MockProviderClient(),
    allowConcurrentSubmit,
  };
  return {
    ...harness,
    chatId: chat.id,
    root,
    deps,
    runner: new TurnRunner(deps),
  };
}

describe("TurnRunner explicit root submit", () => {
  it("keeps null root identity in its task and does not replay old roots", async () => {
    const f = await fixture();
    const input = {
      chatId: f.chatId,
      content: "Edited root",
      parentMessageId: null,
      taskId: "root-edit",
    };
    const submitted = await f.runner.submitMessage(input);
    expect(await f.runner.submitMessage(input)).toEqual(submitted);
    expect(
      (await f.store.tasks.getTask(submitted.runId))?.payload.parentMessageId,
    ).toBeNull();
    expect(
      (await f.store.conversations.getMessage(submitted.userMessageId))
        ?.parentMessageId,
    ).toBeUndefined();
    expect(
      await assembleMessages(
        f.deps,
        f.chatId,
        submitted.assistantMessageId,
        null,
      ),
    ).toEqual([{ role: "user", content: "Edited root" }]);
    await expect(
      f.runner.submitMessage({
        chatId: f.chatId,
        content: "Edited root",
        taskId: "root-edit",
      }),
    ).rejects.toMatchObject({ code: "duplicate_task" });
  });

  it("preserves unfinished-chat serialization for root submissions", async () => {
    const f = await fixture(false);
    const first = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "First root",
      parentMessageId: null,
    });
    await expect(
      f.runner.submitMessage({
        chatId: f.chatId,
        content: "Second root",
        parentMessageId: null,
      }),
    ).rejects.toMatchObject({ code: "chat_busy" });
    expect(
      (await f.store.conversations.listMessages(f.chatId)).map(
        (message) => message.id,
      ),
    ).toEqual([first.userMessageId, first.assistantMessageId]);
    expect(
      await f.store.conversations.listSiblings(first.userMessageId),
    ).toHaveLength(2);
  });
});
