// Responses-lane qualification smoke, NATIVE composition.
//
// The whole runtime is `@agentkit/native`: `NativeStore.open` +
// `store.createService({ jsToolAuthority: { configuredMode: "full_auto" },
// providerConfig: { protocol: "responses", … } })` over a
// `registerProviderConnection` handle — the Rust provider lane performs the
// Responses HTTP/SSE call against the loopback mock below (the recipe is
// `packages/native/tests/provider-lane.test.ts`), the Rust worker executes
// the turn, the JS tool lane runs the registered tools, and the Rust REST
// v1 + SSE server (`service.startHttpServer`, as in
// `examples/desktop-host/src/wiring.ts`) answers the unchanged
// `createAgentKitClient` calls. The mock provider and every assertion the
// TypeScript composition made are preserved; where the native engine
// genuinely does not emit something (the TS-host verification hook —
// `run.verification` — has no native counterpart, see the drain assertions),
// the honest native witness replaces it and says so.
//
// `recoverOnBoot` retired: native service construction crash-reconciles the
// store itself, and proposal settlement here is synchronous inside the tool
// call, so no boot sweep has anything to recover.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { DatabaseSync } from "node:sqlite";
import { NodeSqliteAssistantStore } from "agentkit/adapters-sqlite-node";
import { createAgentKitClient } from "agentkit/client";
import {
  NativeAssistantStore,
  NativeSessionService,
  NativeStore,
} from "@agentkit/native";
import { proposalFixture, assertProposals } from "./proposals-smoke.mjs";

// The Rust store stamps its own application identity (`AKRS`) and its own
// evolving schema version; the frozen TypeScript adapter (schema 10) is a
// separate storage lineage that refuses foreign files and is refused in
// turn. Asserting native ownership is therefore the durable witness the
// old `user_version === 10` check was: the file was created and stamped by
// the engine that owns it, not merely by a pinned migration number.
const NATIVE_APPLICATION_ID = 0x414b5253;
/** 32 bytes: the floor the Rust HTTP backend enforces on `bearerToken`. */
const BEARER_TOKEN = "0123456789abcdef0123456789abcdef";

const secret = "private-encrypted-state";
const sent = [];
let executed = 0;
function message(text) {
  return {
    type: "message",
    id: `message-${sent.length}`,
    role: "assistant",
    status: "completed",
    content: [{ type: "output_text", text }],
  };
}
function call(name, id, input = {}) {
  return {
    type: "function_call",
    id: `item-${id}`,
    call_id: id,
    name,
    namespace: "agentkit",
    arguments: JSON.stringify(input),
    status: "completed",
  };
}
function responseOutput() {
  return sent.length === 1
    ? [
        {
          type: "reasoning",
          id: "reasoning",
          encrypted_content: secret,
          summary: [],
        },
        message("Checking"),
        call("lookup", "call-one"),
        call("write_items", "call-write", { action_id: "write_item_fixture" }),
        call("destroy_items", "call-destroy", {
          action_id: "destroy_item_fixture",
        }),
      ]
    : [message(sent.length === 2 ? "Done" : "After reopen")];
}
// The loopback mock became a real HTTP server because the Rust provider
// lane performs the HTTP/SSE call in Rust: it POSTs `/responses` (SSE,
// `stream: true`, `store: false`, `include: ["reasoning.encrypted_content"]`)
// exactly like the production lane, and this server answers with the same
// `response.completed` frame the old in-process transport returned — the
// Rust Responses state machine accepts a terminal frame carrying the full
// output array. The captured `{ path, headers, body }` entries keep the old
// `sent[i].body.input` assertions verbatim.
async function startProviderMock() {
  const server = Bun.serve({
    hostname: "127.0.0.1",
    port: 0,
    async fetch(request) {
      const url = new URL(request.url);
      if (request.method !== "POST" || url.pathname !== "/responses")
        return new Response("not found", { status: 404 });
      const headers = {};
      request.headers.forEach((value, key) => {
        headers[key.toLowerCase()] = value;
      });
      const body = JSON.parse(await request.text());
      sent.push({ path: url.pathname, headers, body });
      const frame = {
        type: "response.completed",
        response: {
          id: "response",
          status: "completed",
          output: responseOutput(),
          usage: { input_tokens: 2, output_tokens: 3, total_tokens: 5 },
        },
      };
      return new Response(`data: ${JSON.stringify(frame)}\n\ndata: [DONE]\n\n`, {
        headers: { "content-type": "text/event-stream" },
      });
    },
  });
  return {
    baseUrl: `http://127.0.0.1:${server.port}`,
    stop: () => server.stop(),
  };
}
function tool() {
  return {
    definition: {
      name: "lookup",
      version: "1",
      effect: "read",
      capability: "lookup",
      description: "Look up a fixture value",
      inputSchema: { type: "object", additionalProperties: false },
    },
    async execute(context) {
      executed++;
      return {
        ok: true,
        data: { value: "found" },
        modelData: { value: "slim" },
        sources: [],
        warnings: [],
        truncated: false,
        limits: context.limits,
      };
    },
  };
}
async function start(path) {
  const store = await NativeStore.open(path);
  const port = new NativeAssistantStore({ store });
  // The service options carry only the connection id; the mock and the
  // connection itself are process-global below. The native continuation
  // scope binds every encrypted state to the provider connection identity
  // (`connectionId` + `generation`, rust/crates/host/src/chat_history.rs),
  // so the reopen must reuse the ONE registered connection — a second
  // registration would fail the resumed turn with
  // `provider_continuation_scope_mismatch`. The old TypeScript composition
  // re-created its provider per app because its lane keyed continuations by
  // provider id alone; reusing one connection is the faithful equivalent of
  // "the same provider server", not a weakening.
  // `full_auto` is the native equivalent of the old guard-less composition:
  // the Rust authority still judges every JS tool call, it just admits.
  const service = await store.createService({
    jsToolAuthority: { configuredMode: "full_auto" },
    providerConfig: {
      protocol: "responses",
      providerId: "provider",
      connectionId,
      model: "model",
    },
  });
  const proposals = proposalFixture(port, path);
  // The chat is created through REST after this function returns, so the
  // tool execution context closes over a late-bound chat id (the proposal
  // builders stage their proposals against it).
  const chat = { id: undefined };
  for (const t of [tool(), ...proposals.tools]) {
    await service.registerTool(t.definition, (input, request) =>
      t.execute(
        {
          runId: request.runId,
          ...(chat.id === undefined ? {} : { chatId: chat.id }),
          bindings: [],
          limits: { profile: "medium", maxBytes: 256 * 1024, maxItems: 1024 },
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
  const client = createAgentKitClient({
    baseUrl: http.baseUrl,
    headers: () => ({ authorization: `Bearer ${BEARER_TOKEN}` }),
  });
  return {
    port,
    service,
    proposals,
    client,
    chat,
    async stop() {
      await service.stopHttpServer();
      await service.close();
      await store.close();
    },
  };
}
async function authoritativeRun(client, runId) {
  for (let i = 0; i < 250; i++) {
    const run = await client.getRun({ runId });
    if (run.status === "completed") return run;
    assert.ok(
      !["failed", "cancelled", "interrupted"].includes(run.status),
      JSON.stringify(run),
    );
    await new Promise((resolve) => setTimeout(resolve, 20));
  }
  throw new Error("Authoritative run did not complete within 5 seconds");
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
  // before draining: that event belonged to the TypeScript host's
  // verification hook, which the native executor neither runs nor can be
  // installed through `createService` (rust/crates/host/src/runner.rs emits
  // it only for a host-configured hook). The native durable log is settled
  // by `run.completed` itself — which `streamRun` has just proven it
  // contains — so nothing can still be appended when `drainRun` reads it.
  // The drained-log assertions below are the native completeness witness.
  const drained = await app.client.drainRun(result.runId, undefined, {
    signal: AbortSignal.timeout(15000),
  });
  const run = await authoritativeRun(app.client, result.runId);
  const messages = await app.client.listMessages({ chatId });
  const publicJson = JSON.stringify({ run, events, drained, messages });
  assert.ok(!publicJson.includes(secret));
  assert.ok(!publicJson.includes("inputItems"));
  return { runId: result.runId, drained };
}
const path = process.argv[2] ?? "responses.sqlite";
if (process.argv[3]) {
  // The seed branch, stated plainly: the two storage lineages cannot share
  // one file — the Rust store refuses the TypeScript adapter's file at open
  // (`foreign_database`) and the adapter refuses the native file; the
  // native port ships no importer. So the schema-10 continuity a seeded
  // adapter database used to be fed through directly is verified here at
  // the file level through the frozen adapter's own read path (never
  // touched by any native handle), and the full smoke then runs natively
  // against a sidecar database next to the seed.
  const seed = JSON.parse(readFileSync(process.argv[3], "utf8"));
  assert.equal(seed.schema, 10);
  const legacy = new NodeSqliteAssistantStore(path);
  try {
    assert.equal(
      legacy.database.query("PRAGMA user_version").get().user_version,
      10,
    );
    assert.equal(
      (await legacy.conversations.getChat(seed.chatId)).id,
      seed.chatId,
    );
    const messages = await legacy.conversations.listMessages(seed.chatId);
    for (const saved of seed.messages)
      assert.ok(
        messages.some(
          (message) =>
            message.id === saved.id && message.content === saved.content,
        ),
      );
  } finally {
    legacy.close();
  }
}
const nativePath = process.argv[3] ? `${path}.native.sqlite` : path;
// One loopback mock and ONE provider connection serve both apps: the native
// continuation scope binds encrypted state to the connection identity, so
// the reopen resumes on the same connection (see start()).
const provider = await startProviderMock();
const connectionId = await NativeSessionService.registerProviderConnection(
  provider.baseUrl,
  "sk-qualification",
);
const app = await start(nativePath);
{
  // Native-ownership witness for the old `user_version === 10` check (see
  // NATIVE_APPLICATION_ID above): read the file the store owns without
  // taking any write side of it.
  const file = new DatabaseSync(nativePath, { readOnly: true });
  try {
    assert.equal(
      file.prepare("PRAGMA application_id").get().application_id,
      NATIVE_APPLICATION_ID,
    );
    assert.ok(
      file.prepare("PRAGMA user_version").get().user_version > 0,
      "the native store must stamp its schema version",
    );
  } finally {
    file.close();
  }
}
let chatId;
try {
  // The chat is opened as a NATIVE SESSION because this host executes JS
  // write tools: the session carries the durable permission scope (the
  // execution state the guard consults, the allowance binding for the
  // proposal fixture). A REST-minted chat would also run its tools — the
  // guard answers absence with the configured mode since 2026-11-01 — but
  // a host with write tools should be governed, not merely permitted. The
  // session id IS the chatId the REST routes accept (submit/stream/drain/
  // getRun/listMessages all run against it unchanged).
  chatId = crypto.randomUUID();
  await app.service.createSession({
    sessionId: chatId,
    profileId: "responses-smoke",
    model: "model",
  });
  app.proposals.policy.allow({
    chatId,
    toolName: "write_items",
    proposalKind: "write_items",
    scopeKey: "fixture-scope",
    maxRisk: "low",
  });
  const first = await submit(app, chatId, "Find it");
  assert.equal(sent.length, 2);
  assert.equal(executed, 1);
  await assertProposals(app.proposals, app.port, chatId);
  // The verification-pass witness (`drained.some(run.verification)`) had no
  // native counterpart; the honest completeness witness is that the full
  // durable replay carries the tool lifecycle and its own terminator: the
  // `lookup` call was staged, ran, and the run settled.
  assert.ok(
    first.drained.some(
      (event) =>
        event.type === "run.tool.requested" && event.data.toolName === "lookup",
    ),
    JSON.stringify(first.drained.map((event) => event.type)),
  );
  assert.ok(
    first.drained.some((event) => event.type === "run.completed"),
    JSON.stringify(first.drained.map((event) => event.type)),
  );
  assert.ok(
    JSON.stringify(await app.port.continuations.getByRun(first.runId)).includes(
      secret,
    ),
  );
} finally {
  await app.stop();
}
const reopened = await start(nativePath);
try {
  await submit(reopened, chatId, "Continue");
  await assertProposals(reopened.proposals, reopened.port, chatId, false);
  assert.equal(sent.length, 3);
  assert.equal(executed, 1);
  const input = sent[2].body.input;
  assert.equal(input.filter((item) => item.type === "reasoning").length, 1);
  assert.equal(
    input.filter(
      (item) => item.type === "function_call" && item.call_id === "call-one",
    ).length,
    1,
  );
  const outputs = input.filter(
    (item) =>
      item.type === "function_call_output" && item.call_id === "call-one",
  );
  assert.equal(outputs.length, 1);
  assert.ok(outputs[0].output.includes("slim"));
  assert.ok(!outputs[0].output.includes("found"));
  console.log(
    JSON.stringify({
      responses: true,
      requests: sent.length,
      toolExecutions: executed,
      privateContinuationReopened: true,
      publicLeak: false,
    }),
  );
} finally {
  await reopened.stop();
}
await NativeSessionService.revokeProviderConnection(connectionId);
await provider.stop();