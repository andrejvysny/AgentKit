import { describe, expect, it } from "bun:test";
import type { AppendMessageInput, TurnRunnerDeps } from "../src/index.js";
import { assembleMessages } from "../src/turn/history-assembly.js";
import { createHarness } from "./fakes.js";

async function fixture(historyLimit?: number) {
  const harness = createHarness();
  const chat = await harness.store.conversations.createChat({});
  const deps: TurnRunnerDeps = {
    store: harness.store,
    taskRunner: harness.taskRunner,
    clock: harness.clock,
    ids: harness.ids,
    contributors: [],
    providerFactory: () => {
      throw new Error("Assembly does not call a provider");
    },
    ...(historyLimit === undefined ? {} : { historyLimit }),
  };
  const append = (input: Omit<AppendMessageInput, "chatId">) =>
    harness.store.conversations.appendMessage({ chatId: chat.id, ...input });
  const assemble = (anchor = "answer-1") =>
    assembleMessages(deps, chat.id, anchor, "System");
  return { ...harness, chatId: chat.id, append, assemble };
}

async function question(f: Awaited<ReturnType<typeof fixture>>) {
  await f.append({ id: "question-1", role: "user", content: "First question" });
  await f.append({
    id: "answer-1",
    role: "assistant",
    content: "Partial answer",
    runId: "run-1",
    metadata: { placeholder: true },
  });
}

async function toolRound(
  f: Awaited<ReturnType<typeof fixture>>,
  suffix: string,
  parent: string,
) {
  const call = await f.append({
    id: `call-${suffix}`,
    role: "assistant",
    content: "Calling tool",
    runId: "run-1",
    parentMessageId: parent,
    activate: false,
    metadata: { internal: true },
    toolCalls: [{ id: suffix, name: "echo", argumentsJson: "{}" }],
  });
  return f.append({
    id: `result-${suffix}`,
    role: "tool",
    content: `Result ${suffix}`,
    toolCallId: suffix,
    runId: "run-1",
    parentMessageId: call.id,
    activate: false,
    metadata: { internal: true, toolName: "echo" },
  });
}

describe("run-anchored provider history", () => {
  it("excludes later queued user turns and their assistant placeholders", async () => {
    const f = await fixture();
    await question(f);
    await f.append({
      id: "question-2",
      role: "user",
      content: "Later question",
    });
    await f.append({
      id: "answer-2",
      role: "assistant",
      content: "Later answer",
      runId: "run-2",
    });
    expect(await f.assemble()).toEqual([
      { role: "system", content: "System" },
      { role: "user", content: "First question" },
    ]);
  });

  it("resumes its own complete tool chain after another branch becomes active", async () => {
    const f = await fixture();
    await question(f);
    const first = await toolRound(f, "first", "answer-1");
    await toolRound(f, "second", first.id);
    await f.append({
      id: "other-question",
      role: "user",
      content: "Other branch",
      parentMessageId: "question-1",
    });
    await f.append({
      id: "other-answer",
      role: "assistant",
      content: "Other answer",
      runId: "other-run",
    });
    expect((await f.store.conversations.getMessage("answer-1"))?.active).toBe(
      false,
    );
    expect((await f.assemble()).map((message) => message.content)).toEqual([
      "System",
      "First question",
      "Calling tool",
      "Result first",
      "Calling tool",
      "Result second",
    ]);
  });

  it("includes own inactive tool records written after a later queued turn", async () => {
    const f = await fixture();
    await question(f);
    await f.append({ id: "later-user", role: "user", content: "Queued later" });
    await f.append({
      id: "later-answer",
      role: "assistant",
      content: "Queued placeholder",
      runId: "run-2",
    });
    await toolRound(f, "late-tool", "answer-1");
    expect((await f.assemble()).map((message) => message.content)).toEqual([
      "System",
      "First question",
      "Calling tool",
      "Result late-tool",
    ]);
  });

  it("keeps current-run tool chains complete even when they exceed the history window", async () => {
    const f = await fixture(2);
    await question(f);
    const first = await toolRound(f, "first", "answer-1");
    await toolRound(f, "second", first.id);
    const messages = await f.assemble();
    expect(
      messages
        .filter((message) => message.role === "tool")
        .map((message) => message.toolCallId),
    ).toEqual(["first", "second"]);
    expect(
      messages
        .filter((message) => message.role === "user")
        .map((message) => message.content),
    ).toEqual(["First question"]);
  });

  it("preserves completed ancestor runs and their provider-legal tool linkage", async () => {
    const f = await fixture();
    await question(f);
    const previous = await toolRound(f, "previous", "answer-1");
    await f.store.conversations.updateMessage("answer-1", {
      metadata: {},
      content: "First answer",
    });
    await f.append({
      id: "question-2",
      role: "user",
      content: "Follow up",
      parentMessageId: previous.id,
    });
    await f.append({
      id: "answer-2",
      role: "assistant",
      content: "",
      runId: "run-2",
    });
    expect(
      (await f.assemble("answer-2")).map((message) => message.content),
    ).toEqual([
      "System",
      "First question",
      "Calling tool",
      "Result previous",
      "First answer",
      "Follow up",
    ]);
  });

  it("fails closed for missing or foreign anchors", async () => {
    const f = await fixture();
    await expect(f.assemble("missing")).rejects.toMatchObject({
      code: "invalid_history_anchor",
    });
    const other = await f.store.conversations.createChat({});
    await f.store.conversations.appendMessage({
      id: "foreign",
      chatId: other.id,
      role: "assistant",
      content: "Foreign",
    });
    await expect(f.assemble("foreign")).rejects.toMatchObject({
      code: "invalid_history_anchor",
    });
  });

  it("rejects a foreign turn inserted into the current run's internal parent chain", async () => {
    const f = await fixture();
    await question(f);
    const result = await toolRound(f, "first", "answer-1");
    const foreign = await f.append({
      id: "foreign-turn",
      role: "user",
      content: "Foreign turn",
      parentMessageId: "answer-1",
    });
    const record = f.store.conversations.messages.find(
      (message) => message.id === result.id,
    )!;
    record.parentMessageId = foreign.id;
    await expect(f.assemble()).rejects.toMatchObject({
      code: "invalid_history_anchor",
    });
  });

  it("rejects a cyclic ancestor chain instead of looping", async () => {
    const f = await fixture();
    await question(f);
    const questionRecord = f.store.conversations.messages.find(
      (message) => message.id === "question-1",
    )!;
    questionRecord.parentMessageId = "question-1";
    await expect(f.assemble()).rejects.toMatchObject({
      code: "invalid_history_anchor",
    });
  });
});
