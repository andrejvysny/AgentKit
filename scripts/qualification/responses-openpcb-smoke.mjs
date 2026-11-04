// Responses-lane OpenPCB qualification smoke, NATIVE composition.
//
// The whole runtime is `@agentkit/native` — the same cutover
// `responses-smoke.mjs` received, ported to the Node cell: this script runs
// under plain `node` (the qualified Node consumers execute it), so the
// loopback provider mock is `node:http` instead of `Bun.serve`, and the
// durable-store witnesses read through `NativeAssistantStore` instead of
// the frozen TypeScript adapter. The Rust Responses lane POSTs
// `/responses` (SSE, `stream: true`, `store: false`,
// `include: ["reasoning.encrypted_content"]`) to the mock exactly like
// production; the mock answers with the same `response.completed` frame
// the old in-process transport returned, and the captured `{path,headers,
// body}` entries keep the old request-shape assertions verbatim.
//
// Honest reductions (each recorded in docs/rust/parity.md):
//  - The old fixture's `authKind` ("api-key" / "chatgpt-account") was
//    metadata on the in-process TypeScript transport; the native lane
//    presents exactly one wire shape — `authorization: Bearer <key>` —
//    for every registered connection (`rust/crates/napi/src/
//    provider_connection.rs`). Both profiles therefore register keyed
//    connections; the profiles remain independent connections, store
//    files, and request histories, and no assertion ever distinguished
//    them by anything but their own counters.
//  - `run.verification` belonged to the TypeScript host's verification
//    hook, which the native executor neither runs nor can be installed
//    through `createService` (`rust/crates/host/src/runner.rs` emits it
//    only for a host-configured hook). The native durable log is settled
//    by `run.completed` itself, so the drained-log assertions below are
//    the completeness witness, exactly as in `responses-smoke.mjs`.
//  - The retired stack (`TurnRunner`, `TaskService`,
//    `SingleProcessTaskRunner`, `createRestHandler`, `ResponsesClient`,
//    `MemorySecretStore`, `NodeSqliteAssistantStore`) is gone; the fixture
//    system prompt/correction configuration it carried was staged for the
//    retired runner and asserted nothing.
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { createServer } from "node:http";
import { createAgentKitClient } from "agentkit/client";
import {
  NativeAssistantStore,
  NativeSessionService,
  NativeStore,
} from "@agentkit/native";

function readCatalog() {
  // The qualification consumer stages the catalog next to this script as
  // `openpcb-catalog.json` (scripts/qualify-package.mjs); from the repo
  // itself, fall back to the canonical fixture the staging copies.
  const staged = new URL("./openpcb-catalog.json", import.meta.url);
  try {
    return readFileSync(staged, "utf8");
  } catch {
    return readFileSync(
      new URL(
        "../../packages/testing/fixtures/openpcb/catalog.json",
        import.meta.url,
      ),
      "utf8",
    );
  }
}
const catalog = JSON.parse(readCatalog());
const digest = createHash("sha256")
  .update(JSON.stringify(catalog.tools))
  .digest("hex");
assert.equal(catalog.tools.length, 15);
assert.equal(
  digest,
  "22aa56a046f7c3000a3f894eaddb7959297a46e9252950ff1efac87a06e75eaa",
);
assert.equal(digest, catalog.provenance.toolsSha256);
assert.equal(
  catalog.provenance.commit,
  "d69189fa88605e140af01c1246e2da54b04f2201",
);

/** 32 bytes: the floor the Rust HTTP backend enforces on `bearerToken`. */
const BEARER_TOKEN = "0123456789abcdef0123456789abcdef";

const nativeCall = {
  type: "function_call",
  id: "native-provider-item",
  call_id: "openpcb-summary:call/17",
  namespace: "agentkit",
  name: "designer_get_design_summary",
  arguments: '{"designId":"fixture-design"}',
  status: "completed",
};
const reasoning = {
  type: "reasoning",
  id: "native-reasoning",
  summary: [],
  encrypted_content: "openpcb-fixture-private-state",
};
const sent = [];
const executed = [];

function outputText(text) {
  return {
    type: "message",
    id: `answer-${sent.length}`,
    role: "assistant",
    status: "completed",
    content: [{ type: "output_text", text }],
  };
}

/**
 * The loopback Responses mock as a real HTTP server: the Rust lane speaks
 * SSE to it exactly like the production provider, and the per-call script
 * (`sent.length === firstCall` → reasoning + tool call, otherwise the
 * final text) is verbatim the old in-process transport's `request()`.
 */
async function startProviderMock() {
  const server = createServer((incoming, outgoing) => {
    if (
      incoming.method !== "POST" ||
      new URL(incoming.url, "http://127.0.0.1").pathname !== "/responses"
    ) {
      outgoing.writeHead(404);
      outgoing.end("not found");
      return;
    }
    const chunks = [];
    incoming.on("data", (chunk) => chunks.push(chunk));
    incoming.on("end", () => {
      const headers = {};
      for (const [key, value] of Object.entries(incoming.headers))
        headers[key.toLowerCase()] = Array.isArray(value)
          ? value.join(", ")
          : value;
      const body = JSON.parse(Buffer.concat(chunks).toString("utf8"));
      sent.push({ path: incoming.url, headers, body });
      const output =
        sent.length === firstCallOf
          ? [reasoning, outputText("Reading design"), nativeCall]
          : [outputText("17 parts")];
      const frame = {
        type: "response.completed",
        response: {
          id: `response-${sent.length}`,
          status: "completed",
          output,
          usage: { input_tokens: 2, output_tokens: 3, total_tokens: 5 },
        },
      };
      outgoing.writeHead(200, { "content-type": "text/event-stream" });
      outgoing.end(`data: ${JSON.stringify(frame)}\n\ndata: [DONE]\n\n`);
    });
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

// The mock serves both profiles with one global script position: the
// profile driver arms it once — `sent.length === firstCallOf` (checked
// after each push) marks that profile's FIRST call, whose output carries
// the reasoning + native catalog call; every other call answers the final
// text, verbatim the old `provider(authKind, firstCall)` script.
let firstCallOf = 1;

function tools() {
  return catalog.tools.map((definition) => ({
    definition,
    async execute(context, input) {
      assert.equal(definition.name, nativeCall.name);
      assert.deepEqual(input, { designId: "fixture-design" });
      executed.push(input);
      return {
        ok: true,
        data: { localDetail: "host-only" },
        modelData: { designId: "fixture-design", parts: 17 },
        sources: [],
        warnings: [],
        truncated: false,
        limits: context.limits,
      };
    },
  }));
}

async function start(path, authKind, connectionKey) {
  // Open the store first: a keyed registration requires the store's own
  // schema (`register_provider_connection` never creates a database), and
  // the same `{connectionKey, credential}` re-registers idempotently —
  // the reopen phase therefore resumes on the SAME durable connection
  // identity (the native continuation scope binds encrypted state to the
  // provider connection identity, rust/crates/host/src/chat_history.rs).
  const store = await NativeStore.open(path);
  let service;
  try {
    const connectionId = await NativeSessionService.registerProviderConnection(
      provider.baseUrl,
      `sk-qualification-${authKind}`,
      { connectionKey, storePath: path },
    );
    // `full_auto` is the native equivalent of the old guard-less
    // composition: the Rust authority still judges every JS tool call,
    // it just admits.
    service = await store.createService({
      jsToolAuthority: { configuredMode: "full_auto" },
      providerConfig: {
        protocol: "responses",
        providerId: "provider",
        connectionId,
        model: "model",
      },
    });
    const chat = { id: undefined };
    for (const t of tools()) {
      await service.registerTool(t.definition, (input, request) =>
        t.execute(
          {
            runId: request.runId,
            ...(chat.id === undefined ? {} : { chatId: chat.id }),
            bindings: [],
            limits: {
              profile: "medium",
              maxBytes: 256 * 1024,
              maxItems: 1024,
            },
            ...(request.toolCallId === null
              ? {}
              : { toolCallId: request.toolCallId }),
          },
          input,
        ),
      );
    }
    const http = await service.startHttpServer({
      bearerToken: BEARER_TOKEN,
      defaultModel: "model",
    });
    return {
      store,
      chat,
      port: new NativeAssistantStore({ store }),
      connectionId,
      client: createAgentKitClient({
        baseUrl: http.baseUrl,
        headers: () => ({ authorization: `Bearer ${BEARER_TOKEN}` }),
      }),
      async stop() {
        await service.stopHttpServer();
        await service.close();
        await store.close();
      },
    };
  } catch (error) {
    await store.close();
    throw error;
  }
}

async function submit(app, chatId, content) {
  app.chat.id = chatId;
  const { result } = await app.client.submitMessage({ chatId }, { content });
  const events = [];
  for await (const event of app.client.streamRun(result.runId, {
    signal: AbortSignal.timeout(15000),
  }))
    events.push(event);
  assert.ok(
    events.some((event) => event.type === "run.completed"),
    JSON.stringify(events),
  );
  // The old code then polled the store for a `run.verification` event
  // before draining; see the header for why the native executor has no
  // such hook. `streamRun` has just proven the durable log contains its
  // own terminator, so nothing can still be appended when `drainRun`
  // reads it — the drained assertions are the completeness witness.
  const drained = await app.client.drainRun(result.runId, undefined, {
    signal: AbortSignal.timeout(15000),
  });
  for (let attempt = 0; attempt < 250; attempt++) {
    const run = await app.client.getRun({ runId: result.runId });
    if (run.status === "completed") break;
    assert.ok(
      !["failed", "cancelled", "interrupted"].includes(run.status),
      JSON.stringify(run),
    );
    if (attempt === 249)
      throw new Error("Authoritative native catalog run did not complete");
    await new Promise((resolve) => setTimeout(resolve, 20));
  }
  const durableEvents = await app.port.tasks.listEvents(result.runId);
  assert.deepEqual(drained, durableEvents);
  assert.equal(
    new Set(drained.map((event) => event.eventId)).size,
    drained.length,
  );
  assert.deepEqual(
    drained.map((event) => event.seq),
    drained.map((_, index) => index),
  );
  const messages = await app.client.listMessages({ chatId });
  const publicData = JSON.stringify({ events, drained, messages });
  assert.ok(!publicData.includes(reasoning.encrypted_content));
  assert.ok(!publicData.includes("inputItems"));
  return { runId: result.runId, drained };
}

function assertRequests(requests) {
  // Native-equivalent conversion of the old byte-exact catalog assertion:
  // the Rust host stages guard-less `effect: "write"` JS tools as
  // DIRECT-WRITE contributions, which stamp a fixed governance sentence in
  // front of the advertised description (`rust/crates/host/src/governance/
  // staging.rs` `ContributedTool::checked`, pinned by
  // `rust/crates/host/tests/governance/staging.rs`). The old guard-less
  // TypeScript composition advertised no such sentence; the catalog bytes
  // are still pinned exactly — every other field of every tool is
  // compared verbatim, and the read tools' descriptions stay raw.
  const advertised = (tool) =>
    tool.effect === "write"
      ? `Direct write tool; host authorization and domain receipts are required. ${tool.description}`
      : tool.description;
  const expected = [
    {
      type: "namespace",
      name: "agentkit",
      description: "Tools supplied by the application.",
      tools: catalog.tools.map((tool) => ({
        type: "function",
        name: tool.name,
        description: advertised(tool),
        parameters: tool.inputSchema,
        strict: false,
      })),
    },
  ];
  for (const request of requests)
    assert.deepEqual(request.body.tools, expected);
}

function assertContinuation(request) {
  const input = request.body.input;
  assert.deepEqual(
    input.filter((item) => item.type === "reasoning"),
    [reasoning],
  );
  assert.deepEqual(
    input.filter((item) => item.type === "function_call"),
    [nativeCall],
  );
  const outputs = input.filter((item) => item.type === "function_call_output");
  assert.equal(outputs.length, 1);
  assert.equal(outputs[0].call_id, nativeCall.call_id);
  assert.ok(outputs[0].output.includes('"parts":17'));
  assert.ok(!outputs[0].output.includes("host-only"));
}

async function qualifyProfile(authKind, connectionKey) {
  const startIndex = sent.length;
  const executedBefore = executed.length;
  const path = `responses-openpcb-${authKind}.sqlite`;
  const firstCall = startIndex + 1;
  let app = await start(path, authKind, connectionKey);
  let chatId;
  try {
    chatId = (await app.client.createChat({})).id;
    firstCallOf = firstCall;
    const first = await submit(app, chatId, "Read native design summary");
    // The verification-pass witness had no native counterpart; the honest
    // completeness witness is that the full durable replay carries the
    // tool lifecycle and its own terminator: the catalog call was staged,
    // ran, and the run settled.
    assert.ok(
      first.drained.some(
        (event) =>
          event.type === "run.tool.requested" &&
          event.data.toolName === nativeCall.name,
      ),
      JSON.stringify(first.drained.map((event) => event.type)),
    );
    assert.ok(
      first.drained.some((event) => event.type === "run.completed"),
      JSON.stringify(first.drained.map((event) => event.type)),
    );
    assert.equal(sent.length - startIndex, 2);
    assert.equal(executed.length - executedBefore, 1);
    assertContinuation(sent[startIndex + 1]);
    const usage = first.drained.filter((event) => event.type === "run.usage");
    assert.equal(usage.length, 2);
    assert.deepEqual(
      usage.map((event) => event.data.totalTokens),
      [5, 5],
    );
    assert.ok(
      JSON.stringify(
        await app.port.continuations.getByRun(first.runId),
      ).includes(reasoning.encrypted_content),
    );
  } finally {
    await app.stop();
  }
  app = await start(path, authKind, connectionKey);
  try {
    await submit(app, chatId, "Continue after reopen");
    assert.equal(sent.length - startIndex, 3);
    assert.equal(executed.length - executedBefore, 1);
    assertContinuation(sent[startIndex + 2]);
    assertRequests(sent.slice(startIndex));
  } finally {
    await app.stop();
    await NativeSessionService.revokeProviderConnection(app.connectionId);
  }
}

const provider = await startProviderMock();
try {
  await qualifyProfile("api-key", "openpcb-qualification-api-key");
  await qualifyProfile(
    "chatgpt-account",
    "openpcb-qualification-chatgpt-account",
  );
} finally {
  await provider.stop();
}
console.log(
  JSON.stringify({
    openpcbCatalog: true,
    sourceCommit: catalog.provenance.commit,
    toolsSha256: digest,
    nativeTools: 15,
    profiles: ["api-key", "chatgpt-account"],
    requests: sent.length,
    toolExecutions: executed.length,
    privateContinuationReopened: true,
    publicLeak: false,
    pcbMutationQualified: false,
  }),
);
