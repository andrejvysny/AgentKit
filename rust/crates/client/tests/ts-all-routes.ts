import {
  MemoryMcpServerConfigStore,
  MemorySecretStore,
} from "@agentkit/adapters-memory";
import {
  ProposalService,
  ConversationService,
  SessionWritePolicy,
  defaultClock,
  defaultIds,
  type ApplyOutcome,
  type ApplyProposalInput,
  type ProposalApplier,
} from "@agentkit/host";
import type { AiChatRequest } from "@agentkit/core";
import type { AiRunEvent } from "@agentkit/contracts";
import { HangingProviderClient, MockProviderClient } from "@agentkit/testing";
import { createRestHandler } from "@agentkit/transport-http";
import {
  createLiveFixture,
  TEST_CHAT_ID,
} from "../../../../packages/transport-http/tests/support/fixture.ts";

class Provider extends MockProviderClient {
  private readonly hanging = new HangingProviderClient();
  override async *streamChat(input: AiChatRequest): AsyncIterable<AiRunEvent> {
    if (input.messages.at(-1)?.content === "park")
      yield* this.hanging.streamChat(input);
    else yield* super.streamChat(input);
  }
}
class Applier implements ProposalApplier {
  calls = 0;
  private readonly receipts = new Map<string, ApplyOutcome>();
  async apply(input: ApplyProposalInput): Promise<ApplyOutcome> {
    this.calls += 1;
    const outcome: ApplyOutcome = {
      status: "applied",
      appliedOps: input.proposal.operations.length,
      failedOps: [],
    };
    this.receipts.set(input.operationId, outcome);
    return outcome;
  }
  async getOutcome(operationId: string): Promise<ApplyOutcome | null> {
    return this.receipts.get(operationId) ?? null;
  }
}
const provider = new Provider();
provider.setScript(
  [1, 2, 3].map((index) => ({
    steps: [{ kind: "text", content: `Reverse answer ${index}.` }],
  })),
);
const fixture = await createLiveFixture(provider);
const policy = new SessionWritePolicy({ clock: defaultClock });
const applier = new Applier();
const proposals = new ProposalService({
  store: fixture.store,
  applier,
  policy,
  clock: defaultClock,
  ids: defaultIds,
});
for (const id of ["approve-me", "reject-me"]) {
  await proposals.stage({
    id,
    chatId: TEST_CHAT_ID,
    scopeKey: TEST_CHAT_ID,
    toolName: "edit",
    kind: "edit",
    risk: "low",
    operations: [{ op: "insert", text: "ok" }],
  });
}
async function seedInterrupted(): Promise<void> {
  const chatId = "resume-chat";
  const taskId = "resume-me";
  await fixture.store.conversations.createChat({ id: chatId });
  await fixture.store.conversations.appendMessage({
    id: "resume-user",
    chatId,
    runId: taskId,
    role: "user",
    content: "resume question",
  });
  await fixture.store.conversations.appendMessage({
    id: "resume-assistant",
    chatId,
    runId: taskId,
    role: "assistant",
    content: "",
    metadata: { placeholder: true },
  });
  await fixture.store.tasks.createTask({
    taskId,
    kind: "chat.turn",
    scopeId: chatId,
    availableAt: new Date(Date.now() + 60_000).toISOString(),
    payload: {
      chatId,
      model: "m1",
      userMessageId: "resume-user",
      assistantMessageId: "resume-assistant",
    },
  });
  await fixture.store.tasks.transitionTask(taskId, ["queued"], "running");
  await fixture.store.tasks.transitionTask(taskId, ["running"], "interrupted");
}
await seedInterrupted();
const apiToken = crypto.randomUUID();
const evidenceToken = crypto.randomUUID();
const evidence: {
  case: string;
  operation: string;
  method: string;
  path: string;
  status: number;
}[] = [];
const handler = createRestHandler({
  ...fixture.deps,
  conversations: new ConversationService({ store: fixture.store }),
  proposals,
  writePolicy: policy,
  secrets: new MemorySecretStore(),
  mcpConfigs: new MemoryMcpServerConfigStore(),
  authenticate: async (request) =>
    request.headers.get("authorization") === `Bearer ${apiToken}`
      ? { id: "reverse-actor" }
      : new Response(null, { status: 401 }),
  providerOps: {
    async refreshModels(providerId) {
      const models = [
        {
          providerId,
          modelId: "m2",
          displayName: "Refreshed",
          fetchedAt: new Date(0).toISOString(),
        },
      ];
      await fixture.store.providers.replaceModels(providerId, models);
      return models;
    },
    async testConnection() {
      return { ok: true, latencyMs: 1 };
    },
  },
  toolCatalog: {
    async listTools() {
      return [
        {
          namespace: "fixture",
          definition: {
            name: "fixture_read",
            version: "1.0.0",
            effect: "read",
            capability: "read",
            description: "Fixture read",
            inputSchema: { type: "object" },
          },
        },
      ];
    },
  },
});
const server = Bun.serve({
  port: 0,
  hostname: "127.0.0.1",
  idleTimeout: 0,
  async fetch(request) {
    const response = await handler(request);
    evidence.push({
      case: request.headers.get("x-agentkit-case") ?? "stream",
      operation: request.headers.get("x-agentkit-operation") ?? "",
      method: request.method,
      path: new URL(request.url).pathname,
      status: response.status,
    });
    return response;
  },
});
const audit = Bun.serve({
  port: 0,
  hostname: "127.0.0.1",
  fetch(request) {
    if (request.headers.get("authorization") !== `Bearer ${evidenceToken}`)
      return new Response(null, { status: 401 });
    return Response.json({ requests: evidence, applied: applier.calls });
  },
});
console.log(
  JSON.stringify({
    baseUrl: server.url.href,
    token: apiToken,
    evidenceUrl: audit.url.href,
    evidenceToken,
  }),
);
