import assert from "node:assert/strict";
import { createServer } from "node:http";
import { Readable } from "node:stream";
import { pipeline } from "node:stream/promises";
import { NodeSqliteAssistantStore } from "agentkit/adapters-sqlite-node";
import { MemorySecretStore } from "agentkit/adapters-memory";
import { createAgentKitClient } from "agentkit/client";
import {
  ProposalService,
  SessionWritePolicy,
  TaskService,
  TurnRunner,
  defaultClock,
  defaultIds,
  recoverOnBoot,
} from "agentkit/host";
import { SingleProcessTaskRunner } from "agentkit/runner-local";
import { MockProviderClient } from "agentkit/testing";
import { createRestHandler } from "agentkit/transport-http";

async function respond(handler, incoming, outgoing) {
  const controller = new AbortController();
  outgoing.on("close", () => {
    if (!outgoing.writableFinished) controller.abort();
  });
  try {
    const init = {
      method: incoming.method,
      headers: incoming.headers,
      signal: controller.signal,
    };
    if (incoming.method !== "GET" && incoming.method !== "HEAD") {
      init.body = Readable.toWeb(incoming);
      init.duplex = "half";
    }
    const response = await handler(
      new Request(`http://127.0.0.1${incoming.url}`, init),
    );
    outgoing.writeHead(response.status, Object.fromEntries(response.headers));
    if (response.body)
      await pipeline(Readable.fromWeb(response.body), outgoing);
    else outgoing.end();
  } catch (error) {
    if (!outgoing.headersSent) outgoing.writeHead(500);
    outgoing.destroy(error instanceof Error ? error : new Error(String(error)));
  }
}

async function seedProvider(store) {
  if ((await store.providers.listProviders()).length > 0) return;
  await store.providers.upsertProvider({
    id: "fake",
    kind: "openai-compatible",
    label: "Local fake provider",
    baseUrl: "http://127.0.0.1:1",
    defaultModel: "fake-model",
    enabled: true,
  });
  await store.settings.updateSettings({ defaultProviderId: "fake" });
}

export async function serveNodeHandler(handler) {
  const server = createServer((incoming, outgoing) => {
    void respond(handler, incoming, outgoing);
  });
  await new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });
  return {
    origin: `http://127.0.0.1:${server.address().port}`,
    stop: () =>
      new Promise((resolve, reject) =>
        server.close((error) => (error ? reject(error) : resolve())),
      ),
  };
}

async function createRuntime(store) {
  const provider = new MockProviderClient();
  provider.setScript([
    { steps: [{ kind: "text", content: "Local fake reply." }] },
  ]);
  const taskRunner = new SingleProcessTaskRunner({ store, pollMs: 5 });
  const turns = new TurnRunner({
    store,
    taskRunner,
    secrets: new MemorySecretStore(),
    contributors: [],
    clock: defaultClock,
    ids: defaultIds,
    providerFactory: () => provider,
  });
  const tasks = new TaskService({
    store,
    taskRunner,
    clock: defaultClock,
    ids: defaultIds,
  });
  const proposals = new ProposalService({
    store,
    clock: defaultClock,
    ids: defaultIds,
    policy: new SessionWritePolicy(),
    applier: {
      async apply() {
        throw new Error("No write tools in this fixture");
      },
      async getOutcome() {
        return null;
      },
    },
  });
  await recoverOnBoot({ taskRunner, proposals });
  const worker = await taskRunner.startWorker(turns, { concurrency: 1 });
  const handler = createRestHandler({ store, turns, tasks, proposals });
  return { worker, turns, handler };
}

/** This local fixture deliberately exposes no remote bind option or credentials. */
export async function buildNodeHost(dbPath) {
  const store = new NodeSqliteAssistantStore(dbPath);
  let runtime;
  try {
    await seedProvider(store);
    runtime = await createRuntime(store);
  } catch (error) {
    store.close();
    throw error;
  }
  const { worker, turns, handler } = runtime;
  const server = await serveNodeHandler(handler);
  let stopping;
  return {
    store,
    origin: server.origin,
    stop() {
      stopping ??= (async () => {
        await server.stop();
        await worker.stop();
        await turns.disposeContributors();
        store.close();
      })();
      return stopping;
    },
  };
}

export async function exerciseNodeHost(dbPath) {
  const app = await buildNodeHost(dbPath);
  let chat;
  try {
    const client = createAgentKitClient({ baseUrl: app.origin });
    chat = await client.createChat({});
    const { result } = await client.submitMessage(
      { chatId: chat.id },
      { content: "Reply without external services." },
    );
    const types = [];
    for await (const event of client.streamRun(result.runId, {
      signal: AbortSignal.timeout(15000),
    }))
      types.push(event.type);
    assert.ok(types.includes("run.completed"), JSON.stringify(types));
    const page = await client.listMessages({ chatId: chat.id });
    assert.ok(
      page.items.some((message) => message.content === "Local fake reply."),
    );
  } finally {
    await app.stop();
  }
  const reopened = new NodeSqliteAssistantStore(dbPath);
  try {
    assert.equal((await reopened.conversations.getChat(chat.id))?.id, chat.id);
  } finally {
    reopened.close();
  }
  return { chatId: chat.id, terminal: "run.completed", reopened: true };
}
