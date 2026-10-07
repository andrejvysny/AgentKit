import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { NodeSqliteAssistantStore } from "agentkit/adapters-sqlite-node";
import { MemorySecretStore } from "agentkit/adapters-memory";
import { ResponsesClient } from "agentkit/core";
import { createAgentKitClient } from "agentkit/client";
import {
  TaskService,
  TurnRunner,
  defaultClock,
  defaultIds,
} from "agentkit/host";
import { SingleProcessTaskRunner } from "agentkit/runner-local";
import { createRestHandler } from "agentkit/transport-http";
import { serveNodeHandler } from "./node-host.mjs";

const catalog = JSON.parse(
  readFileSync(new URL("./openpcb-catalog.json", import.meta.url), "utf8"),
);
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

function provider(authKind, firstCall) {
  return new ResponsesClient({
    id: "provider",
    transport: {
      identity: {
        providerId: "provider",
        protocol: "responses",
        connectionId: `fixture-${authKind}`,
        generation: 1,
      },
      authKind,
      async request(input) {
        sent.push(input);
        const output =
          sent.length === firstCall
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
        return new Response(`data: ${JSON.stringify(frame)}\n\n`);
      },
    },
  });
}

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

function createTurns(store, taskRunner, authKind, firstCall) {
  return new TurnRunner({
    store,
    taskRunner,
    secrets: new MemorySecretStore(),
    clock: defaultClock,
    ids: defaultIds,
    providerFactory: () => provider(authKind, firstCall),
    contributors: [{ namespace: "openpcb", contribute: async () => tools() }],
    context: {
      listBindings: async () => [],
      systemPrompt: async () => "Pinned OpenPCB catalog fixture",
    },
    correction: { maxPasses: 0 },
    verification: {
      async verify() {
        return {
          status: "pass",
          checks: [{ id: "fixture", ok: true }],
          deficiencies: [],
        };
      },
    },
  });
}

async function start(path, authKind, firstCall) {
  const store = new NodeSqliteAssistantStore(path);
  await store.providers.upsertProvider({
    id: "provider",
    label: "Mock Responses native catalog",
    kind: "responses",
    baseUrl: "https://api.openai.com/v1",
    defaultModel: "model",
    enabled: true,
  });
  await store.settings.updateSettings({ defaultProviderId: "provider" });
  const taskRunner = new SingleProcessTaskRunner({ store, pollMs: 5 });
  const turns = createTurns(store, taskRunner, authKind, firstCall);
  const tasks = new TaskService({
    store,
    taskRunner,
    clock: defaultClock,
    ids: defaultIds,
  });
  const worker = await taskRunner.startWorker(turns, { concurrency: 1 });
  const server = await serveNodeHandler(
    createRestHandler({ store, turns, tasks }),
  );
  return {
    store,
    client: createAgentKitClient({ baseUrl: server.origin }),
    async stop() {
      await server.stop();
      await worker.stop();
      await turns.disposeContributors();
      store.close();
    },
  };
}

async function submit(app, chatId, content) {
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
  const drained = await app.client.drainRun(result.runId, undefined, {
    signal: AbortSignal.timeout(15000),
  });
  const durableEvents = await app.store.tasks.listEvents(result.runId);
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
  const expected = [
    {
      type: "namespace",
      name: "agentkit",
      description: "Tools supplied by the application.",
      tools: catalog.tools.map((tool) => ({
        type: "function",
        name: tool.name,
        description: tool.description,
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

async function qualifyProfile(authKind) {
  const startIndex = sent.length;
  const executedBefore = executed.length;
  const path = `responses-openpcb-${authKind}.sqlite`;
  const firstCall = startIndex + 1;
  let app = await start(path, authKind, firstCall);
  let chatId;
  try {
    chatId = (await app.client.createChat({})).id;
    const first = await submit(app, chatId, "Read native design summary");
    assert.ok(first.drained.some((event) => event.type === "run.verification"));
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
        await app.store.continuations.getByRun(first.runId),
      ).includes(reasoning.encrypted_content),
    );
  } finally {
    await app.stop();
  }
  app = await start(path, authKind, firstCall);
  try {
    await submit(app, chatId, "Continue after reopen");
    assert.equal(sent.length - startIndex, 3);
    assert.equal(executed.length - executedBefore, 1);
    assertContinuation(sent[startIndex + 2]);
    assertRequests(sent.slice(startIndex));
  } finally {
    await app.stop();
  }
}

await qualifyProfile("api-key");
await qualifyProfile("chatgpt-account");
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
