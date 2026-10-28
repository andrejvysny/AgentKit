import { expect } from "bun:test";
import type { AiTool, AiToolExecutionContext } from "@agentkit/core";
import { MockProviderClient } from "@agentkit/testing";
import { SqliteAssistantStore } from "../../../adapters-sqlite/src/index.js";
import {
  AgentSessionService,
  TaskService,
  TurnRunner,
  defaultClock,
  defaultIds,
  type AgentInputRecord,
  type Clock,
} from "../../src/index.js";
import { FakeTaskRunner } from "../fakes.js";

export interface State {
  effects: number;
  crash?: "before_body" | "after_effect" | "after_result";
  fullAuto?: boolean;
  failOnce?: boolean;
  timeoutOnce?: boolean;
  toolAvailable?: boolean;
  guardDenied?: boolean;
}

export interface FixtureOptions {
  clock?: Clock;
  afterRunning?: (eventId: string) => Promise<void>;
  afterAdmission?: (executionId: string) => Promise<void>;
  afterEffect?: (ctx: AiToolExecutionContext) => Promise<void>;
  afterResult?: (executionId: string) => Promise<void>;
}

export async function open(
  path: string,
  state: State,
  legacyRoot?: string,
  options: FixtureOptions = {},
) {
  const runtimeRoot = legacyRoot ?? process.env.AGENTKIT_TEST_PACKAGE_ROOT;
  const clock = options.clock ?? defaultClock;
  const host =
    runtimeRoot === undefined
      ? { TurnRunner, TaskService, AgentSessionService }
      : ((await import(
          `${runtimeRoot}/dist/host/index.js`
        )) as typeof import("../../src/index.js"));
  const adapter =
    runtimeRoot === undefined
      ? { SqliteAssistantStore }
      : ((await import(
          `${runtimeRoot}/dist/adapters-sqlite/index.js`
        )) as typeof import("../../../adapters-sqlite/src/index.js"));
  const testing =
    runtimeRoot === undefined
      ? { MockProviderClient }
      : ((await import(
          `${runtimeRoot}/dist/testing/index.js`
        )) as typeof import("@agentkit/testing"));
  const store = new adapter.SqliteAssistantStore(path, { clock });
  const originalAppendEvents = store.tasks.appendEvents.bind(store.tasks);
  store.tasks.appendEvents = async (taskId, events, fence) => {
    await originalAppendEvents(taskId, events, fence);
    for (const event of events)
      if (event.type === "run.tool.running")
        await options.afterRunning?.(event.eventId);
  };
  const taskRunner = new FakeTaskRunner();
  const mock = new testing.MockProviderClient();
  mock.setScript([{ steps: [{ kind: "text", content: "done" }] }]);
  const interrupt = async (runId: string): Promise<never> => {
    await store.tasks.interruptTask!(runId, "simulated process death");
    throw new Error("simulated process death");
  };
  const tool: AiTool = {
    definition: {
      name: "write",
      version: "1",
      capability: "write",
      effect: "write",
      description: "Count one effect",
      inputSchema: { type: "object" },
      ...(state.timeoutOnce ? { timeoutMs: 50 } : {}),
    },
    async execute(ctx) {
      state.effects += 1;
      await options.afterEffect?.(ctx);
      if (state.failOnce) {
        state.failOnce = false;
        throw new Error("failure after effect");
      }
      if (state.timeoutOnce) {
        state.timeoutOnce = false;
        await new Promise<void>((_resolve, reject) =>
          ctx.signal!.addEventListener(
            "abort",
            () => reject(new Error("timeout after effect")),
            { once: true },
          ),
        );
      }
      if (state.crash === "after_effect") {
        state.crash = undefined;
        await interrupt(ctx.runId);
      }
      return {
        ok: true,
        data: { effects: state.effects },
        modelData: { effects: state.effects },
        sources: [],
        warnings: [],
        truncated: false,
        limits: ctx.limits,
      };
    },
  };
  const originalTransaction = store.transaction.bind(store);
  store.transaction = async (operation) => {
    const result = await originalTransaction(operation);
    if (typeof result === "string" && /^[a-f0-9]{64}$/.test(result))
      await options.afterAdmission?.(result);
    if (
      state.crash === "before_body" &&
      typeof result === "string" &&
      /^[a-f0-9]{64}$/.test(result)
    ) {
      state.crash = undefined;
      const run = (await store.tasks.listByScope("chat")).find(
        (task) => task.status === "running",
      )!;
      await interrupt(run.taskId);
    }
    return result;
  };
  const originalComplete = store.toolExecutions?.complete.bind(
    store.toolExecutions,
  );
  if (originalComplete)
    store.toolExecutions.complete = async (id, result, fence) => {
      await originalComplete(id, result, fence);
      await options.afterResult?.(id);
      if (state.crash === "after_result") {
        state.crash = undefined;
        await interrupt(fence.taskId);
      }
    };
  const runner = new host.TurnRunner({
    store,
    taskRunner,
    providerFactory: () => mock,
    contributors: [
      {
        namespace: "test",
        contribute: async () => (state.toolAvailable === false ? [] : [tool]),
      },
    ],
    toolGuards: [
      {
        canExecute: () =>
          state.guardDenied
            ? { allowed: false, reason: "policy changed: mutation disabled" }
            : state.fullAuto
              ? { allowed: true }
              : { allowed: false, reason: "approval required", approval: {} },
      },
    ],
    clock,
    ids: defaultIds,
  });
  const tasks = new host.TaskService({
    store,
    taskRunner,
    clock,
    ids: defaultIds,
  });
  const service = new host.AgentSessionService({
    store,
    taskRunner,
    turns: runner,
    tasks,
    clock,
    ids: defaultIds,
  });
  if (!(await store.providers.getProvider("provider"))) {
    await store.providers.upsertProvider({
      id: "provider",
      label: "Mock",
      kind: "openai-compatible",
      baseUrl: "http://localhost:1",
      defaultModel: "model",
      enabled: true,
    });
    await store.settings.updateSettings({ defaultProviderId: "provider" });
    await service.createSession({
      sessionId: "chat",
      profile: { id: "coding" },
      providerId: "provider",
      model: "model",
    });
  }
  return { store, mock, runner, service, clock };
}

type Fixture = Awaited<ReturnType<typeof open>>;
export async function drive(f: Fixture): Promise<boolean> {
  const claim = await f.store.tasks.claimNext({
    ownerId: "worker",
    scopesBusy: [],
    now: f.clock.now(),
  });
  if (!claim) return false;
  await f.runner.execute({
    taskId: claim.task.taskId,
    attemptId: claim.attempt.attemptId,
    leaseToken: claim.lease.leaseToken,
    signal: new AbortController().signal,
  });
  return true;
}

export async function start(f: Fixture): Promise<string> {
  f.mock.setScript([
    {
      steps: [
        {
          kind: "tool_call",
          toolCallId: "original-call",
          name: "write",
          argumentsJson: "{}",
        },
      ],
    },
  ]);
  const sent = await f.service.send("chat", { content: "write once" });
  if (sent.kind !== "turn") throw new Error("expected turn");
  await drive(f);
  return sent.turnId;
}

export async function pending(f: Fixture): Promise<AgentInputRecord> {
  const requests = await f.store.inputs.listInputs({
    sessionId: "chat",
    kinds: ["human_request"],
    statuses: ["pending"],
  });
  expect(requests).toHaveLength(1);
  return requests[0]!;
}

export async function resume(f: Fixture, runId: string): Promise<void> {
  await f.store.tasks.transitionTask(runId, ["interrupted"], "queued");
  await drive(f);
}
