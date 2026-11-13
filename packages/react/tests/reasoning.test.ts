import "./support/dom.js";
import { act, renderHook, waitFor } from "@testing-library/react";
import { expect, test } from "bun:test";
import { OpenAiCompatibleClient } from "@agentkit/core";
import type { AgentKitClient } from "@agentkit/client";
import type {
  AiRunMessageReasoningDeltaEvent,
  TaskEventEnvelope,
} from "@agentkit/contracts";
import {
  startNativeServer,
  type NativeServer,
} from "../../../rust/tests/support/http-server.js";
import { startProviderBridge } from "../../client/tests/support/provider-bridge.js";
import { useChat } from "../src/index.js";
import { wrapper } from "./support/render.js";

function interceptedProvider() {
  let release = (): void => {};
  const gate = new Promise<void>((resolve) => {
    release = resolve;
  });
  const encode = (value: unknown): Uint8Array =>
    new TextEncoder().encode(`data: ${JSON.stringify(value)}\n\n`);
  const fetchImpl: typeof fetch = Object.assign(
    async () =>
      new Response(
        new ReadableStream<Uint8Array>({
          async start(controller) {
            controller.enqueue(
              encode({
                choices: [{ delta: { reasoning_content: "Think Ž🙂" } }],
              }),
            );
            await gate;
            controller.enqueue(
              encode({ choices: [{ delta: { reasoning: " harder" } }] }),
            );
            controller.enqueue(
              encode({
                choices: [
                  { delta: { content: "Answer" }, finish_reason: "stop" },
                ],
              }),
            );
            controller.enqueue(new TextEncoder().encode("data: [DONE]\n\n"));
            controller.close();
          },
        }),
        { headers: { "content-type": "text/event-stream" } },
      ),
    { preconnect() {} },
  );
  return {
    release,
    provider: new OpenAiCompatibleClient({
      id: "provider",
      kind: "openai-compatible",
      baseUrl: "http://unused.invalid/v1",
      fetchImpl,
    }),
  };
}

/** The durable event log, read through the fixture's private control listener. */
async function storedEvents(
  server: NativeServer,
  runId: string,
  options?: unknown,
): Promise<TaskEventEnvelope[]> {
  const response = await fetch(`${server.controlUrl}/tasks`, {
    method: "POST",
    headers: {
      authorization: `Bearer ${server.controlToken}`,
      "content-type": "application/json",
    },
    body: JSON.stringify({ operation: "listEvents", taskId: runId, options }),
  });
  if (!response.ok) throw new Error(`Native test control: ${response.status}`);
  return (await response.json()) as TaskEventEnvelope[];
}

function mountChat(client: AgentKitClient, chatId: string) {
  return renderHook(() => useChat(chatId), { wrapper: wrapper(client) });
}

function expectMessage(
  rendered: ReturnType<typeof mountChat>,
  reasoning: string,
  content: string,
): void {
  expect(
    rendered.result.current.messages.at(-1)?.metadata.reasoningContent,
  ).toBe(reasoning);
  expect(rendered.result.current.messages.at(-1)?.content).toBe(content);
}

async function streamTurn(
  rendered: ReturnType<typeof mountChat>,
  release: () => void,
  releaseTerminal: () => void,
): Promise<string> {
  await waitFor(() => expect(rendered.result.current.status).toBe("idle"));
  await act(async () => {
    await rendered.result.current.submit("Explain");
  });
  const runId = rendered.result.current.activeRunId;
  if (!runId) throw new Error("Expected submitted run");
  await waitFor(() => expectMessage(rendered, "Think Ž🙂", ""));
  release();
  await waitFor(() =>
    expect(rendered.result.current.messages.at(-1)?.content).toBe("Answer"),
  );
  expect(rendered.result.current.status).toBe("streaming");
  expectMessage(rendered, "Think Ž🙂 harder", "Answer");
  releaseTerminal();
  await waitFor(() => expect(rendered.result.current.status).toBe("idle"));
  expectMessage(rendered, "Think Ž🙂 harder", "Answer");
  return runId;
}

test("provider SSE reasoning streams through the real host, survives SQLite reopen and React reload exactly once", async () => {
  const { provider, release } = interceptedProvider();
  const bridge = startProviderBridge(provider);
  let server = await startNativeServer({
    compatibility: true,
    providerUrl: bridge.url,
    providerToken: bridge.token,
  });
  // The fixture reads its own durable log through the bridge's test socket.
  bridge.setEventsReader((runId, options) =>
    storedEvents(server, runId, options),
  );
  const chat = await server.client.createChat({});
  const terminal = holdTerminal(server.client);
  let rendered = mountChat(terminal.client, chat.id);
  try {
    const runId = await streamTurn(rendered, release, terminal.release);
    const events = await storedEvents(server, runId);
    expect(
      events
        .filter(
          (event): event is AiRunMessageReasoningDeltaEvent =>
            event.type === "run.message.reasoning-delta",
        )
        .map((event) => event.data.delta),
    ).toEqual(["Think Ž🙂", " harder"]);
    rendered.unmount();
    server = await server.restart();
    rendered = mountChat(server.client, chat.id);
    await waitFor(() => expect(rendered.result.current.status).toBe("idle"));
    expectMessage(rendered, "Think Ž🙂 harder", "Answer");
    const replayed = [];
    for await (const event of server.client.streamRun(runId)) {
      if (event.type === "run.message.reasoning-delta")
        replayed.push(event.data.delta);
    }
    expect(replayed).toEqual(["Think Ž🙂", " harder"]);
  } finally {
    release();
    terminal.release();
    rendered.unmount();
    await server.stop();
    await bridge.stop();
  }
}, 60_000);

function holdTerminal(client: AgentKitClient) {
  let release = (): void => {};
  const gate = new Promise<void>((resolve) => {
    release = resolve;
  });
  return {
    release,
    client: {
      ...client,
      async *streamRun(...args: Parameters<AgentKitClient["streamRun"]>) {
        for await (const event of client.streamRun(...args)) {
          if (event.type === "run.completed") await gate;
          yield event;
        }
      },
    },
  };
}
