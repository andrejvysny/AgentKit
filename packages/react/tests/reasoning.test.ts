import "./support/dom.js";
import { act, renderHook, waitFor } from "@testing-library/react";
import { expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { SqliteAssistantStore } from "@agentkit/adapters-sqlite";
import { OpenAiCompatibleClient, type AiProviderClient } from "@agentkit/core";
import { createAgentKitClient, type AgentKitClient } from "@agentkit/client";
import type { AiRunMessageReasoningDeltaEvent } from "@agentkit/contracts";
import {
  ChatTurnExecutor,
  ExecutorRegistry,
  TaskService,
  TurnRunner,
  createDispatchingWorker,
  defaultClock,
  defaultIds,
} from "@agentkit/host";
import { SingleProcessTaskRunner } from "@agentkit/runner-local";
import { createRestHandler } from "@agentkit/transport-http";
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

async function seedStore(path: string) {
  const store = new SqliteAssistantStore(path);
  if (!(await store.conversations.getChat("chat"))) {
    await store.conversations.createChat({ id: "chat" });
    await store.providers.upsertProvider({
      id: "provider",
      label: "Provider",
      kind: "openai-compatible",
      baseUrl: "http://unused.invalid/v1",
      defaultModel: "model",
      enabled: true,
    });
    await store.settings.updateSettings({ defaultProviderId: "provider" });
  }
  return store;
}

async function createServices(
  store: SqliteAssistantStore,
  provider: AiProviderClient,
) {
  const runner = new SingleProcessTaskRunner({
    store,
    clock: defaultClock,
    pollMs: 5,
    shutdownMode: "cancel",
  });
  const turns = new TurnRunner({
    store,
    taskRunner: runner,
    providerFactory: () => provider,
    contributors: [],
    clock: defaultClock,
    ids: defaultIds,
  });
  const tasks = new TaskService({
    store,
    taskRunner: runner,
    clock: defaultClock,
    ids: defaultIds,
  });
  const executors = new ExecutorRegistry();
  executors.register(new ChatTurnExecutor(turns));
  const worker = await runner.startWorker(
    createDispatchingWorker(executors, { store, clock: defaultClock }),
    { concurrency: 1, ownerId: "react-reasoning" },
  );
  return { turns, tasks, worker };
}

async function durableServer(path: string, provider: AiProviderClient) {
  const store = await seedStore(path);
  const { turns, tasks, worker } = await createServices(store, provider);
  const server = Bun.serve({
    port: 0,
    fetch: createRestHandler({
      store,
      turns,
      tasks,
      streaming: { pollIntervalMs: 5, heartbeatIntervalMs: 60_000 },
    }),
    idleTimeout: 0,
  });
  const client = createAgentKitClient({
    baseUrl: `http://127.0.0.1:${server.port}`,
  });
  return {
    store,
    client,
    async stop() {
      await worker.stop();
      await server.stop(true);
      store.close();
    },
  };
}

function mountChat(client: AgentKitClient) {
  return renderHook(() => useChat("chat"), { wrapper: wrapper(client) });
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
  const directory = mkdtempSync(join(tmpdir(), "agentkit-react-reasoning-"));
  const path = join(directory, "state.sqlite");
  const { provider, release } = interceptedProvider();
  let server = await durableServer(path, provider);
  const terminal = holdTerminal(server.client);
  let rendered = mountChat(terminal.client);
  try {
    const runId = await streamTurn(rendered, release, terminal.release);
    const events = await server.store.tasks.listEvents(runId);
    expect(
      events
        .filter(
          (event): event is AiRunMessageReasoningDeltaEvent =>
            event.type === "run.message.reasoning-delta",
        )
        .map((event) => event.data.delta),
    ).toEqual(["Think Ž🙂", " harder"]);
    rendered.unmount();
    await server.stop();
    server = await durableServer(path, provider);
    rendered = mountChat(server.client);
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
    rmSync(directory, { recursive: true, force: true });
  }
}, 15_000);

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
