// The Node-runnable composition — the qualified Node cell of this example.
//
// NATIVE COMPOSITION. Like `src/wiring.ts` (the Bun host), every runtime
// decision executes in Rust through `@agentkit/native`: `NativeStore.open`
// + `store.createService({ providerConfig })` over a
// `NativeSessionService.registerProviderConnection` handle, the Rust
// chat-completions lane performs the HTTP/SSE call against the loopback
// fake below, the Rust worker executes the turn, and the Rust REST v1 +
// SSE server (`service.startHttpServer`) answers the unchanged
// `createAgentKitClient` calls. The retired TypeScript backend stack
// (`TurnRunner`, `TaskService`, `SingleProcessTaskRunner`,
// `createRestHandler`) is gone: Node loads the `.node` addon directly —
// the recipe is `scripts/qualification/responses-smoke.mjs`, ported to
// plain Node (`node:http` replaces `Bun.serve` for the fake provider).
//
// `recoverOnBoot` retired too: native service construction crash-reconciles
// the store itself, so no boot sweep has anything to recover.
//
// The fixture keeps its old contract: no remote bind option, no credentials
// beyond the loopback bearer token the Rust server enforces itself.
import assert from "node:assert/strict";
import { createServer } from "node:http";
import {
  NativeAssistantStore,
  NativeSessionService,
  NativeStore,
} from "agentkit/native";
import { createAgentKitClient } from "agentkit/client";

/** 32 bytes: the floor the Rust HTTP backend enforces on `bearerToken`. */
const BEARER_TOKEN = "0123456789abcdef0123456789abcdef";
const FAKE_REPLY = "Local fake reply.";

/**
 * The loopback fake provider as a real HTTP server: the Rust provider lane
 * performs the chat-completions HTTP/SSE call in Rust (it POSTs
 * `/chat/completions` with `stream: true`), exactly like the production
 * lane, and this server answers with the `delta.content` →
 * `finish_reason: stop` → `[DONE]` SSE the Rust stream parser expects —
 * the verbatim replacement for the old in-process `MockProviderClient`
 * script of the same single text reply.
 */
async function startFakeProvider() {
  const server = createServer((incoming, outgoing) => {
    if (
      incoming.method !== "POST" ||
      new URL(incoming.url, "http://127.0.0.1").pathname !== "/chat/completions"
    ) {
      outgoing.writeHead(404);
      outgoing.end("not found");
      return;
    }
    incoming.resume(); // drain the request body; the fixture ignores it
    outgoing.writeHead(200, { "content-type": "text/event-stream" });
    const write = (choice) =>
      outgoing.write(`data: ${JSON.stringify({ choices: [choice] })}\n\n`);
    write({ delta: { content: FAKE_REPLY } });
    write({ delta: {}, finish_reason: "stop" });
    outgoing.write("data: [DONE]\n\n");
    outgoing.end();
  });
  await new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });
  return {
    baseUrl: `http://127.0.0.1:${server.address().port}`,
    stop: () =>
      new Promise((resolve, reject) =>
        server.close((error) => (error ? reject(error) : resolve())),
      ),
  };
}

async function createService(store) {
  const provider = await startFakeProvider();
  let connectionId;
  let service;
  try {
    connectionId = await NativeSessionService.registerProviderConnection(
      provider.baseUrl,
      "sk-node-host",
    );
    service = await store.createService({
      // The fixture registers no tools, so the authority never has to
      // judge anything; `full_auto` matches the old guard-less composition.
      jsToolAuthority: { configuredMode: "full_auto" },
      providerConfig: {
        protocol: "chat-completions",
        providerId: "fake",
        connectionId,
        model: "fake-model",
      },
    });
  } catch (error) {
    await provider.stop();
    throw error;
  }
  const http = await service.startHttpServer({
    bearerToken: BEARER_TOKEN,
    defaultModel: "fake-model",
  });
  return {
    provider,
    service,
    origin: http.baseUrl,
    client: createAgentKitClient({
      baseUrl: http.baseUrl,
      headers: () => ({ authorization: `Bearer ${BEARER_TOKEN}` }),
    }),
  };
}

/** This local fixture deliberately exposes no remote bind option or credentials. */
export async function buildNodeHost(dbPath) {
  const store = await NativeStore.open(dbPath);
  let runtime;
  try {
    runtime = await createService(store);
  } catch (error) {
    await store.close();
    throw error;
  }
  const { provider, service, origin, client } = runtime;
  let stopping;
  return {
    store,
    client,
    origin,
    stop() {
      stopping ??= (async () => {
        await service.stopHttpServer();
        await service.close();
        await store.close();
        await provider.stop();
      })();
      return stopping;
    },
  };
}

export async function exerciseNodeHost(dbPath) {
  const app = await buildNodeHost(dbPath);
  let chat;
  try {
    chat = await app.client.createChat({});
    const { result } = await app.client.submitMessage(
      { chatId: chat.id },
      { content: "Reply without external services." },
    );
    const types = [];
    for await (const event of app.client.streamRun(result.runId, {
      signal: AbortSignal.timeout(15000),
    }))
      types.push(event.type);
    assert.ok(types.includes("run.completed"), JSON.stringify(types));
    const page = await app.client.listMessages({ chatId: chat.id });
    assert.ok(page.items.some((message) => message.content === FAKE_REPLY));
  } finally {
    await app.stop();
  }
  // Durability witness: reopen the NATIVE store file (the engine that
  // stamped it owns it — the frozen TypeScript adapter is a separate
  // storage lineage and never touches a native file).
  const reopened = await NativeStore.open(dbPath);
  try {
    const port = new NativeAssistantStore({ store: reopened });
    assert.equal((await port.conversations.getChat(chat.id))?.id, chat.id);
  } finally {
    await reopened.close();
  }
  return { chatId: chat.id, terminal: "run.completed", reopened: true };
}
