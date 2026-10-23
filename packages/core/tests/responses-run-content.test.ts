import { expect, test } from "bun:test";
import type { AiChatRequest, AiProviderContinuation } from "../src/index.js";
import {
  call,
  collect,
  harness,
  request,
  terminal,
} from "./responses-helpers.js";

const scope = { chatId: "chat", branchId: "branch" };
const selected: AiChatRequest["runContent"] = {
  messages: [
    { role: "system", content: "SKILL_ONLY_THIS_RUN" },
    { role: "user", content: "TEMPLATE_USER" },
    { role: "assistant", content: "TEMPLATE_ASSISTANT" },
  ],
};
const reasoning = {
  type: "reasoning",
  id: "reasoning",
  encrypted_content: "private-state",
  summary: [],
};

async function initialState(): Promise<AiProviderContinuation> {
  let state: AiProviderContinuation | undefined;
  const environment = harness([terminal([reasoning, call("one")])]);
  const events = await collect(environment.client, {
    ...request,
    runContent: selected,
    continuationScope: scope,
    onContinuation(value) {
      state = value;
    },
  });
  expect(events.find((event) => event.type === "run.failed")).toBeUndefined();
  if (!state) throw new Error("Missing continuation");
  return JSON.parse(JSON.stringify(state)) as AiProviderContinuation;
}

function afterTool(): AiChatRequest["messages"] {
  return [
    ...request.messages,
    {
      role: "assistant",
      content: "",
      toolCalls: [{ id: "one", name: "lookup", argumentsJson: "{}" }],
    },
    { role: "tool", content: "result", toolCallId: "one", name: "lookup" },
  ];
}

test("selected content uses canonical digest, pins same-run bytes, and replays once", async () => {
  const previous = await initialState();
  expect(previous.messageCount).toBe(2);
  expect(previous.runContent).toMatchObject({ runId: request.runId });
  expect(previous.runContent?.digest).toMatch(/^[a-f0-9]{64}$/);
  let current: AiProviderContinuation | undefined;
  const environment = harness();
  const events = await collect(environment.client, {
    ...request,
    messages: afterTool(),
    runContent: selected,
    continuation: previous,
    continuationScope: scope,
    onContinuation(value) {
      current = value;
    },
  });
  expect(events.find((event) => event.type === "run.failed")).toBeUndefined();
  const body = environment.sent[0]?.body;
  expect(body?.instructions).toBe("SKILL_ONLY_THIS_RUN");
  expect(
    ((body?.input ?? []) as unknown[]).filter((item) =>
      JSON.stringify(item).includes("TEMPLATE_USER"),
    ),
  ).toHaveLength(1);
  expect(
    ((body?.input ?? []) as unknown[]).filter((item) =>
      JSON.stringify(item).includes("TEMPLATE_ASSISTANT"),
    ),
  ).toHaveLength(1);
  expect(body?.input).toContainEqual(reasoning);
  expect(current?.messageCount).toBe(4);
  expect(current?.runContent).toEqual(previous.runContent);
});

test("same-run content removal or change fails before transport without dropping reasoning", async () => {
  const previous = await initialState();
  for (const changed of [
    undefined,
    { messages: [] },
    { messages: [{ role: "system" as const, content: "changed" }] },
  ]) {
    const environment = harness();
    const events = await collect(environment.client, {
      ...request,
      messages: afterTool(),
      runContent: changed,
      continuation: previous,
      continuationScope: scope,
    });
    expect(
      events.find((event) => event.type === "run.failed")?.data,
    ).toMatchObject({ errorCode: "continuation_history_mismatch" });
    expect(environment.sent).toHaveLength(0);
  }
});

test("next run retains prior reasoning and template messages while using only current instructions", async () => {
  const previous = await initialState();
  for (const currentSelection of [
    undefined,
    {
      messages: [
        { role: "system" as const, content: "NEW_SKILL" },
        { role: "user" as const, content: "NEW_TEMPLATE" },
      ],
    },
  ]) {
    const environment = harness();
    let current: AiProviderContinuation | undefined;
    const events = await collect(environment.client, {
      ...request,
      runId: "next-run",
      messages: afterTool(),
      runContent: currentSelection,
      continuation: previous,
      continuationScope: scope,
      onContinuation(value) {
        current = value;
      },
    });
    expect(events.find((event) => event.type === "run.failed")).toBeUndefined();
    const body = environment.sent[0]?.body;
    expect(JSON.stringify(body)).not.toContain("SKILL_ONLY_THIS_RUN");
    expect(body?.instructions).toBe(currentSelection ? "NEW_SKILL" : undefined);
    expect(body?.input).toContainEqual(reasoning);
    expect(JSON.stringify(body?.input).split("TEMPLATE_USER")).toHaveLength(2);
    expect(JSON.stringify(body?.input).split("NEW_TEMPLATE")).toHaveLength(
      currentSelection ? 2 : 1,
    );
    expect(current?.runContent?.runId).toBe(
      currentSelection ? "next-run" : undefined,
    );
  }
});

test("selected context does not relax canonical history validation", async () => {
  const previous = await initialState();
  const environment = harness();
  const messages = afterTool();
  messages[0] = { role: "user", content: "tampered" };
  const events = await collect(environment.client, {
    ...request,
    messages,
    runContent: selected,
    continuation: previous,
    continuationScope: scope,
  });
  expect(
    events.find((event) => event.type === "run.failed")?.data,
  ).toMatchObject({ errorCode: "continuation_history_mismatch" });
  expect(environment.sent).toHaveLength(0);
});

test("malformed or oversized selected context fails before transport", async () => {
  for (const invalid of [
    null,
    { messages: "bad" },
    { messages: [null] },
    { messages: [{ role: "tool", content: "bad" }] },
    { messages: [{ role: "user", content: 1 }] },
    { messages: [{ role: "user", content: "x".repeat(1024 * 1024 + 1) }] },
  ]) {
    const environment = harness();
    const events = await collect(environment.client, {
      ...request,
      runContent: invalid as unknown as AiChatRequest["runContent"],
    });
    expect(
      events.find((event) => event.type === "run.failed")?.data,
    ).toMatchObject({
      errorCode:
        invalid &&
        typeof invalid.messages === "object" &&
        JSON.stringify(invalid).length > 1024 * 1024
          ? "state_limit_exceeded"
          : "invalid_request",
    });
    expect(environment.sent).toHaveLength(0);
  }
});
