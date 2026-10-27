/**
 * Profile fields the host now enforces (ADR 0015 §3): model, reasoning effort,
 * per-iteration call cap, output tokens, attempt wall clock, project
 * instructions and skills. Each used to be declared and ignored.
 */
import { describe, expect, it } from "bun:test";
import type { AiRunEvent, RunContentSelection } from "@agentkit/contracts";
import type { AiChatRequest, AiProviderClient, AiTool } from "@agentkit/core";
import { MockProviderClient, type MockTurn } from "@agentkit/testing";
import {
  BUILT_IN_PROFILES,
  SESSION_METADATA_KEY,
  TurnRunner,
  createProfileRegistry,
  mergeProfileSkills,
  readSessionDescriptor,
  type AgentProfile,
  type ProjectInstructionsSource,
  type RunContentProvider,
  type ToolSetContributor,
} from "../src/index.js";
import { FakeAgentInputStore, createHarness } from "./fakes.js";

const chatProfile = BUILT_IN_PROFILES.find(
  (p) => p.kind === "chat",
) as AgentProfile;

function profileWith(patch: Partial<AgentProfile>): AgentProfile {
  return {
    ...chatProfile,
    id: "test/wiring",
    tools: ["echo"],
    resources: {},
    ...patch,
  };
}

class Recording implements AiProviderClient {
  readonly id = "test";
  readonly kind = "openai-compatible" as const;
  readonly requests: AiChatRequest[] = [];
  constructor(readonly inner: AiProviderClient) {}
  async capabilities() {
    return this.inner.capabilities();
  }
  async listModels() {
    return this.inner.listModels();
  }
  streamChat(input: AiChatRequest): AsyncIterable<AiRunEvent> {
    this.requests.push(input);
    return this.inner.streamChat(input);
  }
}

/** Never answers; ends only when the run's signal aborts, like a real socket. */
class ParkedClient extends Recording {
  override streamChat(input: AiChatRequest): AsyncIterable<AiRunEvent> {
    this.requests.push(input);
    return (async function* () {
      await new Promise<void>((resolve) => {
        if (input.signal?.aborted) resolve();
        input.signal?.addEventListener("abort", () => resolve(), {
          once: true,
        });
      });
    })();
  }
}

/** Never settles and never looks at its signal: only a race can end it. */
class DeafClient extends Recording {
  override streamChat(input: AiChatRequest): AsyncIterable<AiRunEvent> {
    this.requests.push(input);
    return (async function* () {
      await new Promise<void>(() => {});
    })();
  }
}

const echo: AiTool = {
  definition: {
    name: "echo",
    version: "1",
    effect: "read",
    capability: "echo",
    description: "Echo.",
    inputSchema: { type: "object", properties: {} },
  },
  async execute(ctx) {
    return {
      ok: true,
      data: {},
      sources: [],
      warnings: [],
      truncated: false,
      limits: ctx.limits,
    };
  },
} as AiTool;
const contributor: ToolSetContributor = {
  namespace: "test",
  contribute: async () => [echo],
};

interface Options {
  profile: AgentProfile;
  script?: MockTurn[];
  descriptor?: Record<string, unknown>;
  submit?: {
    model?: string;
    reasoningEffort?: "low" | "high";
    contentSelection?: RunContentSelection;
  };
  parked?: boolean | "deaf";
  withBudgets?: boolean;
  projectInstructions?: ProjectInstructionsSource;
  runContent?: RunContentProvider;
}

async function run(options: Options) {
  const harness = createHarness();
  harness.store.inputs = new FakeAgentInputStore(harness.clock);
  await harness.store.providers.upsertProvider({
    id: "p1",
    label: "Mock",
    kind: "openai-compatible",
    baseUrl: "http://localhost:1234",
    defaultModel: "provider-default",
    enabled: true,
  });
  await harness.store.settings.updateSettings({ defaultProviderId: "p1" });
  const mock = new MockProviderClient();
  mock.setScript(
    options.script ?? [{ steps: [{ kind: "text", content: "ok" }] }],
  );
  const client =
    options.parked === "deaf"
      ? new DeafClient(mock)
      : options.parked
        ? new ParkedClient(mock)
        : new Recording(mock);
  const runner = new TurnRunner({
    store: harness.store,
    taskRunner: harness.taskRunner,
    providerFactory: () => client,
    contributors: [contributor],
    profiles: createProfileRegistry([options.profile]),
    clock: harness.clock,
    ids: harness.ids,
    ...(options.withBudgets ? { executionBudgets: {} } : {}),
    ...(options.projectInstructions
      ? { projectInstructions: options.projectInstructions }
      : {}),
    ...(options.runContent ? { runContent: options.runContent } : {}),
  });
  const chat = await harness.store.conversations.createChat({
    id: "s1",
    metadata: {
      [SESSION_METADATA_KEY]: {
        version: 1,
        profile: { id: options.profile.id },
        createdAt: "2026-01-01T00:00:00.000Z",
        ...options.descriptor,
      },
    },
  });
  const submitted = await runner.submitMessage({
    chatId: chat.id,
    content: "go",
    ...options.submit,
  });
  const attempt = await harness.store.tasks.createAttempt({
    attemptId: harness.ids.attemptId(),
    taskId: submitted.runId,
    ownerId: "w1",
  });
  const lease = await harness.store.tasks.acquireLease({
    taskId: submitted.runId,
    attemptId: attempt.attemptId,
    ownerId: "w1",
    ttlMs: 60_000,
  });
  await runner
    .execute({
      taskId: submitted.runId,
      attemptId: attempt.attemptId,
      leaseToken: lease.leaseToken,
      signal: new AbortController().signal,
    })
    .catch(() => undefined);
  const events = (await harness.store.tasks.listEvents(
    submitted.runId,
  )) as AiRunEvent[];
  return { harness, client, events, taskId: submitted.runId };
}

describe("profile reasoning effort and model", () => {
  it("uses the profile's effort and model when the session pins none", async () => {
    const f = await run({
      profile: profileWith({
        reasoningEffort: "medium",
        model: "profile-model",
      }),
    });
    expect(f.client.requests[0]?.reasoningEffort).toBe("medium");
    expect(f.client.requests[0]?.model).toBe("profile-model");
  });

  it("a session pin outranks the profile", async () => {
    const f = await run({
      profile: profileWith({
        reasoningEffort: "medium",
        model: "profile-model",
      }),
      submit: { model: "pinned", reasoningEffort: "high" },
    });
    expect(f.client.requests[0]?.reasoningEffort).toBe("high");
    expect(f.client.requests[0]?.model).toBe("pinned");
  });

  it("leaves effort off and keeps the provider default without either", async () => {
    const f = await run({ profile: profileWith({}) });
    expect(f.client.requests[0]?.reasoningEffort).toBeUndefined();
    expect(f.client.requests[0]?.model).toBe("provider-default");
  });

  it("a malformed descriptor effort invalidates the descriptor", () => {
    const meta = (reasoningEffort: unknown) => ({
      [SESSION_METADATA_KEY]: {
        version: 1,
        profile: { id: "x" },
        createdAt: "t",
        reasoningEffort,
      },
    });
    expect(readSessionDescriptor(meta("low"))?.reasoningEffort).toBe("low");
    expect(readSessionDescriptor(meta("extreme"))).toBeUndefined();
  });
});

describe("profile limits", () => {
  it("maxOutputTokens reaches the provider request", async () => {
    const f = await run({
      profile: profileWith({ limits: { maxOutputTokens: 321 } }),
    });
    expect(f.client.requests[0]?.maxOutputTokens).toBe(321);
  });

  it("maxToolCallsPerIteration caps the calls executed per iteration", async () => {
    const f = await run({
      profile: profileWith({ limits: { maxToolCallsPerIteration: 1 } }),
      script: [
        {
          steps: [
            {
              kind: "tool_call",
              toolCallId: "c1",
              name: "echo",
              argumentsJson: "{}",
            },
            {
              kind: "tool_call",
              toolCallId: "c2",
              name: "echo",
              argumentsJson: "{}",
            },
          ],
        },
        { steps: [{ kind: "text", content: "done" }] },
      ],
    });
    const completed = f.events.filter((e) => e.type === "run.tool.succeeded");
    const failed = f.events.filter((e) => e.type === "run.tool.failed");
    expect(completed).toHaveLength(1);
    expect(failed).toHaveLength(1);
    expect(JSON.stringify(failed[0])).toContain(
      "exceeded maxToolCallsPerIteration=1",
    );
  });

  for (const withBudgets of [true, false]) {
    it(`attemptBudgetMs fails a parked provider as failed (host budgets: ${withBudgets})`, async () => {
      const f = await run({
        profile: profileWith({ limits: { attemptBudgetMs: 30 } }),
        parked: true,
        withBudgets,
      });
      const terminal = f.events.find(
        (e) => e.type === "run.failed" || e.type === "run.cancelled",
      );
      expect(terminal?.type).toBe("run.failed");
      const task = await f.harness.store.tasks.getTask(f.taskId);
      expect(task?.status).toBe("failed");
    });

    it(`attemptBudgetMs ends a provider that ignores its signal (host budgets: ${withBudgets})`, async () => {
      const f = await run({
        profile: profileWith({ limits: { attemptBudgetMs: 30 } }),
        parked: "deaf",
        withBudgets,
      });
      const task = await f.harness.store.tasks.getTask(f.taskId);
      expect(task?.status).toBe("failed");
    });
  }
});

describe("profile resources", () => {
  const source = (seen: string[]): ProjectInstructionsSource => ({
    async read(_root, name) {
      seen.push(name);
      return { text: "use bun" };
    },
  });
  const workspace = { workspace: { root: "/repo", trusted: true } };

  it("loads project instructions by default and skips them when false", async () => {
    const seenDefault: string[] = [];
    await run({
      profile: profileWith({ resources: {} }),
      descriptor: workspace,
      projectInstructions: source(seenDefault),
    });
    expect(seenDefault.length).toBeGreaterThan(0);

    const seenOff: string[] = [];
    const f = await run({
      profile: profileWith({ resources: { projectInstructions: false } }),
      descriptor: workspace,
      projectInstructions: source(seenOff),
    });
    expect(seenOff).toEqual([]);
    expect(JSON.stringify(f.client.requests[0]?.messages)).not.toContain(
      "use bun",
    );
  });

  it("merges profile skills after the session's, deduped by id", async () => {
    const selections: unknown[] = [];
    const runContent: RunContentProvider = {
      async resolve(selection) {
        selections.push(structuredClone(selection));
        return { blocks: [], messages: [] };
      },
    };
    await run({
      profile: profileWith({
        resources: {
          skills: [
            { id: "a", revision: "profile-rev" },
            { id: "b", revision: "1" },
          ],
        },
      }),
      submit: {
        contentSelection: { skills: [{ id: "a", revision: "session-rev" }] },
      },
      runContent,
    });
    expect(selections[0]).toEqual({
      skills: [
        { id: "a", revision: "session-rev" },
        { id: "b", revision: "1" },
      ],
    });
  });

  it("mergeProfileSkills leaves the selection alone without profile skills", () => {
    const selection = { skills: [{ id: "a", revision: "1" }] };
    expect(mergeProfileSkills(selection, profileWith({}))).toBe(selection);
    expect(mergeProfileSkills(undefined, undefined)).toBeUndefined();
  });
});
