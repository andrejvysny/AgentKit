/**
 * A session's `turnKind` routes its turns to a host-chosen task kind: send,
 * queued follow-ups, active-turn detection, listing, and fork all use it.
 */
import { afterEach, describe, expect, it } from "bun:test";
import { MemoryAssistantStore } from "@agentkit/adapters-memory";
import {
  AgentSessionService,
  ChatTurnExecutor,
  ExecutorRegistry,
  TaskService,
  TurnRunner,
  createDispatchingWorker,
  defaultClock,
  defaultIds,
  type WorkerHandle,
} from "@agentkit/host";
import { MockProviderClient } from "@agentkit/testing";
import { SingleProcessTaskRunner } from "../src/index.js";
import { waitFor } from "./support/task-runner-harness.js";

const KIND = "app.code.turn";
const handles: WorkerHandle[] = [];

afterEach(async () => {
  for (const handle of handles.splice(0)) await handle.stop();
});

async function setup(workerKind: string = KIND) {
  const store = new MemoryAssistantStore();
  await store.providers.upsertProvider({
    id: "p1",
    label: "Mock",
    kind: "openai-compatible",
    baseUrl: "http://localhost:1234",
    defaultModel: "m1",
    enabled: true,
  });
  await store.settings.updateSettings({ defaultProviderId: "p1" });
  const provider = new MockProviderClient();
  provider.setScript([
    { steps: [{ kind: "text", content: "one" }] },
    { steps: [{ kind: "text", content: "two" }] },
  ]);
  const runner = new SingleProcessTaskRunner({ store, pollMs: 5 });
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
  const service = new AgentSessionService({
    store,
    turns,
    tasks,
    taskRunner: runner,
    clock: defaultClock,
    ids: defaultIds,
  });
  const registry = new ExecutorRegistry();
  registry.register(new ChatTurnExecutor(turns, workerKind));
  const worker = createDispatchingWorker(registry, { store });
  return {
    store,
    service,
    start: async () => {
      handles.push(await runner.startWorker(worker, { concurrency: 1 }));
    },
  };
}

const create = (
  service: AgentSessionService,
  turnKind?: string,
  sessionId = "s1",
) =>
  service.createSession({
    sessionId,
    profile: { id: "default" },
    providerId: "p1",
    model: "m-x",
    ...(turnKind === undefined ? {} : { turnKind }),
  });

describe("session turnKind", () => {
  it("runs send and a queued follow-up under the session kind", async () => {
    const { store, service, start } = await setup();
    await create(service, KIND);
    const first = await service.send("s1", { content: "hi" });
    if (first.kind !== "turn") throw new Error("expected a turn");
    expect((await store.tasks.getTask(first.turnId))?.kind).toBe(KIND);
    expect((await service.getState("s1")).activeTurn?.turnId).toBe(
      first.turnId,
    );

    const queued = await service.send("s1", {
      content: "more",
      inputId: "in1",
      whenBusy: "follow_up",
    });
    expect(queued.kind).toBe("queued");

    await start();
    await waitFor(
      async () => {
        const list = await service.listTurns("s1");
        return list.length === 2 && list.every((t) => t.status === "completed");
      },
      "both turns completed",
      5_000,
    );
    const all = await store.tasks.listByScope("s1");
    expect(all.map((t) => t.kind)).toEqual([KIND, KIND]);
    expect((await service.getState("s1")).activeTurn).toBeUndefined();
  });

  it("default session still uses chat.turn", async () => {
    const { store, service } = await setup("chat.turn");
    await create(service);
    expect((await service.getState("s1")).descriptor.turnKind).toBeUndefined();
    const sent = await service.send("s1", { content: "hi" });
    if (sent.kind !== "turn") throw new Error("expected a turn");
    expect((await store.tasks.getTask(sent.turnId))?.kind).toBe("chat.turn");
  });

  it("rejects an invalid turnKind", async () => {
    const { service } = await setup();
    for (const bad of ["", "chat.custom", "agentkit.x", "Bad Kind", "a..b"]) {
      const error = await create(service, bad).catch((e: unknown) => e);
      expect((error as { code?: string }).code).toBe("invalid_input");
    }
  });

  it("fork inherits turnKind", async () => {
    const { service, start } = await setup();
    await create(service, KIND);
    await start();
    const sent = await service.send("s1", { content: "hi" });
    if (sent.kind !== "turn") throw new Error("expected a turn");
    await waitFor(
      async () => (await service.listTurns("s1"))[0]?.status === "completed",
      "turn completed",
      5_000,
    );
    const forked = await service.fork("s1", sent.assistantMessageId);
    expect(forked.descriptor.turnKind).toBe(KIND);
  });
});
