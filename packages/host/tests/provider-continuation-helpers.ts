import {
  ResponsesClient,
  type ResponsesTransportRequest,
  type AiTool,
} from "@agentkit/core";
import {
  TurnRunner,
  type AssistantStore,
  type TurnRunnerDeps,
} from "../src/index.js";
import { FakeTaskRunner, createTestClock, createTestIds } from "./fakes.js";
import { defaultIds } from "../src/ports/system.js";
import { MemoryAssistantStore } from "../../adapters-memory/src/index.js";

export const reasoning = {
  type: "reasoning",
  id: "reasoning",
  encrypted_content: "private-encrypted-state",
  summary: [],
};
export function outputText(content: string, id = "message") {
  return {
    type: "message",
    id,
    role: "assistant",
    status: "completed",
    content: [{ type: "output_text", text: content }],
  };
}
export function toolCall(id = "call-one") {
  return {
    type: "function_call",
    id: `item-${id}`,
    call_id: id,
    name: "lookup",
    namespace: "agentkit",
    arguments: "{}",
    status: "completed",
  };
}
export function response(output: Record<string, unknown>[]) {
  return {
    type: "response.completed",
    response: {
      id: "response",
      status: "completed",
      output,
      usage: { input_tokens: 2, output_tokens: 3, total_tokens: 5 },
    },
  };
}
export function eventStream(events: unknown[]): Response {
  const bytes = new TextEncoder().encode(
    events.map((event) => `data: ${JSON.stringify(event)}\n\n`).join(""),
  );
  return new Response(
    new ReadableStream<Uint8Array>({
      start(controller) {
        controller.enqueue(bytes);
        controller.close();
      },
    }),
  );
}

interface FixtureOptions {
  store?: AssistantStore;
  overrides?: Partial<TurnRunnerDeps>;
  output?: (call: number, input: ResponsesTransportRequest) => unknown[];
  existingChat?: string;
}

async function fixtureStore(options: FixtureOptions) {
  const clock = createTestClock();
  const ids = options.existingChat ? defaultIds : createTestIds();
  const store = options.store ?? new MemoryAssistantStore({ clock, ids });
  const chatId =
    options.existingChat ??
    (await store.conversations.createChat({ id: "chat" })).id;
  await store.providers.upsertProvider({
    id: "provider",
    label: "Responses",
    kind: "responses",
    baseUrl: "https://api.openai.com/v1",
    defaultModel: "model",
    enabled: true,
  });
  await store.settings.updateSettings({ defaultProviderId: "provider" });
  return { clock, ids, store, chatId };
}

function fixtureTool(onExecute: () => void): AiTool {
  return {
    definition: {
      name: "lookup",
      version: "1",
      effect: "read",
      capability: "lookup",
      description: "Look up a value",
      inputSchema: { type: "object", additionalProperties: false },
    },
    async execute(context) {
      onExecute();
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

function fixtureProvider(
  sent: ResponsesTransportRequest[],
  output: FixtureOptions["output"],
  generation: number,
) {
  return new ResponsesClient({
    id: "provider",
    transport: {
      identity: {
        providerId: "provider",
        protocol: "responses",
        connectionId: "account",
        generation,
      },
      authKind: "chatgpt-account",
      async request(input) {
        sent.push(input);
        return eventStream(
          output?.(sent.length, input) ?? [
            response(
              sent.length === 1
                ? [reasoning, outputText("Checking", "intro"), toolCall()]
                : [outputText("Done")],
            ),
          ],
        );
      },
    },
  });
}

export async function fixture(options: FixtureOptions = {}) {
  const { clock, ids, store, chatId } = await fixtureStore(options);
  const sent: ResponsesTransportRequest[] = [];
  let generation = 1;
  let executed = 0;
  const tool = fixtureTool(() => {
    executed++;
  });
  const deps: TurnRunnerDeps = {
    store,
    taskRunner: new FakeTaskRunner(),
    clock,
    ids,
    contributors: [{ namespace: "test", contribute: async () => [tool] }],
    context: {
      listBindings: async () => [],
      systemPrompt: async () => "Stable prompt",
    },
    providerFactory: () => fixtureProvider(sent, options.output, generation),
    ...options.overrides,
  };
  const runner = new TurnRunner(deps);
  return {
    store,
    runner,
    sent,
    clock,
    ids,
    deps,
    chatId,
    generation: (value: number) => {
      generation = value;
    },
    executed: () => executed,
  };
}

export async function execute(
  runner: TurnRunner,
  store: AssistantStore,
  runId: string,
  signal = new AbortController().signal,
) {
  const task = await store.tasks.getTask(runId);
  const claim = await store.tasks.claimNext({
    ownerId: "test",
    now: new Date(task?.availableAt ?? Date.now()),
    scopesBusy: [],
  });
  if (!claim || claim.task.taskId !== runId)
    throw new Error("Unexpected task claim");
  const ctx = {
    task: claim.task,
    attemptId: claim.attempt.attemptId,
    leaseToken: claim.lease.leaseToken,
    signal,
  };
  await runner.executeTask(ctx);
  return ctx;
}

export async function submitRun(
  environment: Awaited<ReturnType<typeof fixture>>,
  content = "Find it",
) {
  const submitted = await environment.runner.submitMessage({
    chatId: environment.chatId,
    content,
  });
  await execute(environment.runner, environment.store, submitted.runId);
  return submitted;
}
