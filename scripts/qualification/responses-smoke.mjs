import assert from "node:assert/strict";
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
  recoverOnBoot,
} from "agentkit/host";
import { SingleProcessTaskRunner } from "agentkit/runner-local";
import { createRestHandler } from "agentkit/transport-http";
import { proposalFixture, assertProposals } from "./proposals-smoke.mjs";
import { serveNodeHandler } from "./node-host.mjs";

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
function provider() {
  return new ResponsesClient({
    id: "provider",
    transport: {
      identity: {
        providerId: "provider",
        protocol: "responses",
        connectionId: "account",
        generation: 1,
      },
      authKind: "chatgpt-account",
      async request(input) {
        sent.push(input);
        const output = responseOutput();
        const frame = {
          type: "response.completed",
          response: {
            id: "response",
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
function createTurns(store, taskRunner, proposals) {
  return new TurnRunner({
    store,
    taskRunner,
    secrets: new MemorySecretStore(),
    clock: defaultClock,
    ids: defaultIds,
    providerFactory: provider,
    contributors: [
      {
        namespace: "test",
        contribute: async () => [tool(), ...proposals.tools],
      },
    ],
    context: {
      listBindings: async () => [],
      systemPrompt: async () => "Stable prompt",
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
async function start(path) {
  const store = new NodeSqliteAssistantStore(path);
  await store.providers.upsertProvider({
    id: "provider",
    label: "Mock Responses",
    kind: "responses",
    baseUrl: "https://api.openai.com/v1",
    defaultModel: "model",
    enabled: true,
  });
  await store.settings.updateSettings({ defaultProviderId: "provider" });
  const taskRunner = new SingleProcessTaskRunner({ store, pollMs: 5 });
  const proposals = proposalFixture(store, path);
  const turns = createTurns(store, taskRunner, proposals);
  const tasks = new TaskService({
    store,
    taskRunner,
    clock: defaultClock,
    ids: defaultIds,
  });
  await recoverOnBoot({ taskRunner, proposals: proposals.service });
  const worker = await taskRunner.startWorker(turns, { concurrency: 1 });
  const server = await serveNodeHandler(
    createRestHandler({ store, turns, tasks, proposals: proposals.service }),
  );
  return {
    store,
    proposals,
    client: createAgentKitClient({ baseUrl: server.origin }),
    async stop() {
      await server.stop();
      await worker.stop();
      await turns.disposeContributors();
      store.close();
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
  for (
    let i = 0;
    events.some((event) => event.type === "run.tool.requested") && i < 250;
    i++
  ) {
    if (
      (await app.store.tasks.listEvents(result.runId)).some(
        (event) => event.type === "run.verification",
      )
    )
      break;
    await new Promise((resolve) => setTimeout(resolve, 20));
  }
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
const app = await start(path);
assert.equal(
  app.store.database.query("PRAGMA user_version").get().user_version,
  9,
);
if (process.argv[3]) {
  const seed = JSON.parse(readFileSync(process.argv[3], "utf8"));
  assert.equal(seed.schema, 8);
  assert.equal(
    (await app.store.conversations.getChat(seed.chatId)).id,
    seed.chatId,
  );
  const messages = await app.store.conversations.listMessages(seed.chatId);
  for (const saved of seed.messages)
    assert.ok(
      messages.some(
        (message) =>
          message.id === saved.id && message.content === saved.content,
      ),
    );
}
let chatId;
try {
  chatId = (await app.client.createChat({})).id;
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
  await assertProposals(app.proposals, app.store, chatId);
  assert.ok(first.drained.some((event) => event.type === "run.verification"));
  assert.ok(
    JSON.stringify(
      await app.store.continuations.getByRun(first.runId),
    ).includes(secret),
  );
} finally {
  await app.stop();
}
const reopened = await start(path);
try {
  await submit(reopened, chatId, "Continue");
  await assertProposals(reopened.proposals, reopened.store, chatId, false);
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
