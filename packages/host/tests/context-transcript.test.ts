/**
 * Agent-session context assembly (ADR 0017 part 1): transcript replay, the
 * token budget, project instructions and carried-over Skills. A chat without a
 * session descriptor must assemble exactly as before.
 */
import { describe, expect, it } from "bun:test";
import type { AiChatMessage, AiRunEvent } from "@agentkit/contracts";
import type { AiChatRequest, AiTool } from "@agentkit/core";
import { MockProviderClient } from "@agentkit/testing";
import {
  AgentSessionService,
  PromptTemplateService,
  SESSION_METADATA_KEY,
  SkillService,
  TaskService,
  TurnRunner,
  bundledSkillSource,
  createRunContentProvider,
  fitToBudget,
  resolveContextWindow,
  type ProjectInstructionsSource,
  type ToolSetContributor,
  type TurnRunnerDeps,
} from "../src/index.js";
import { defaultTokenEstimator } from "@agentkit/core";
import {
  FakeAgentInputStore,
  FakeTaskRunner,
  createHarness,
  type TestHarness,
} from "./fakes.js";
import { MemoryPromptTemplateStore } from "../../adapters-memory/src/memory-prompt-template-store.js";

class SpyProvider extends MockProviderClient {
  readonly requests: AiChatMessage[][] = [];
  override streamChat(input: AiChatRequest): AsyncIterable<AiRunEvent> {
    this.requests.push(structuredClone(input.messages));
    return super.streamChat(input);
  }
}

interface Fixture extends TestHarness {
  runner: TurnRunner;
  mock: SpyProvider;
  chatId: string;
  inputs: FakeAgentInputStore;
}

interface SetupOptions {
  session?: boolean | Record<string, unknown>;
  overrides?: Partial<TurnRunnerDeps>;
  models?: { modelId: string; contextWindowTokens?: number }[];
}

async function setup(options: SetupOptions = {}): Promise<Fixture> {
  const { session = true } = options;
  const harness = createHarness();
  const mock = new SpyProvider();
  const echo: AiTool<{ text?: string }, Record<string, never>> = {
    definition: {
      name: "echo",
      version: "1.0.0",
      effect: "read",
      capability: "echo",
      description: "Echo the input.",
      inputSchema: { type: "object", properties: { text: { type: "string" } } },
    },
    async execute(ctx) {
      return {
        ok: true as const,
        data: {},
        sources: [],
        warnings: [],
        truncated: false,
        limits: (ctx as unknown as { limits: never }).limits,
      };
    },
  };
  const contributor: ToolSetContributor = {
    namespace: "test",
    contribute: async () => [echo as AiTool],
  };
  await harness.store.providers.upsertProvider({
    id: "p1",
    label: "Mock",
    kind: "openai-compatible",
    baseUrl: "http://localhost:1234",
    defaultModel: "m1",
    enabled: true,
  });
  await harness.store.settings.updateSettings({ defaultProviderId: "p1" });
  if (options.models) {
    await harness.store.providers.replaceModels(
      "p1",
      options.models.map((model) => ({
        providerId: "p1",
        displayName: null,
        fetchedAt: "2026-01-01T00:00:00.000Z",
        ...model,
      })),
    );
  }
  const inputs = new FakeAgentInputStore(harness.clock);
  harness.store.inputs = inputs;
  const chat = await harness.store.conversations.createChat({
    id: "chat-1",
    ...(session === false
      ? {}
      : {
          metadata: {
            [SESSION_METADATA_KEY]: {
              version: 1,
              profile: { id: "default" },
              providerId: "p1",
              model: "m1",
              createdAt: "2026-01-01T00:00:00.000Z",
              ...(typeof session === "object" ? session : {}),
            },
          },
        }),
  });
  const runner = new TurnRunner({
    store: harness.store,
    taskRunner: harness.taskRunner,
    providerFactory: () => mock,
    contributors: [contributor],
    clock: harness.clock,
    ids: harness.ids,
    ...options.overrides,
  });
  return { ...harness, runner, mock, chatId: chat.id, inputs };
}

async function drive(f: Fixture, taskId: string): Promise<void> {
  const attempt = await f.store.tasks.createAttempt({
    attemptId: f.ids.attemptId(),
    taskId,
    ownerId: "worker-1",
  });
  const lease = await f.store.tasks.acquireLease({
    taskId,
    attemptId: attempt.attemptId,
    ownerId: "worker-1",
    ttlMs: 60_000,
  });
  await f.runner.execute({
    taskId,
    attemptId: attempt.attemptId,
    leaseToken: lease.leaseToken,
    signal: new AbortController().signal,
  });
}

interface Turn {
  runId: string;
  assistantMessageId: string;
}

/**
 * Submit one turn and run it. `after` hangs the new user message under that
 * placeholder, the shape a follow-up converted at settlement has (its prior
 * run's records stay off the active path); without it the turn follows the
 * active leaf like a plain `send`.
 */
async function turn(f: Fixture, content: string, after?: Turn): Promise<Turn> {
  const { runId, assistantMessageId } = await f.runner.submitMessage({
    chatId: f.chatId,
    content,
    ...(after === undefined
      ? {}
      : { parentMessageId: after.assistantMessageId }),
  });
  await drive(f, runId);
  return { runId, assistantMessageId };
}

function toolThenAnswer(id: string, answer: string) {
  return [
    {
      steps: [
        {
          kind: "tool_call" as const,
          toolCallId: id,
          name: "echo",
          argumentsJson: `{"text":"${id}"}`,
        },
      ],
    },
    { steps: [{ kind: "text" as const, content: answer }] },
  ];
}

async function warnings(f: Fixture, runId: string) {
  return ((await f.store.tasks.listEvents(runId)) as AiRunEvent[]).flatMap(
    (event) => (event.type === "run.warning" ? [event.data] : []),
  );
}

/** Role plus a short label per message, to assert order without noise. */
function shape(messages: readonly AiChatMessage[]): string[] {
  return messages.map((message) => {
    if (message.role === "tool") return `tool:${message.toolCallId}`;
    const calls = message.toolCalls?.map((call) => call.id).join(",");
    const text =
      typeof message.content === "string"
        ? message.content.slice(0, 24)
        : "[parts]";
    return calls ? `assistant:calls(${calls})` : `${message.role}:${text}`;
  });
}

function expectBalanced(messages: readonly AiChatMessage[]): void {
  const declared = new Set(
    messages.flatMap((m) => m.toolCalls?.map((c) => c.id) ?? []),
  );
  const answered = new Set(
    messages.flatMap((m) => (m.role === "tool" ? [m.toolCallId] : [])),
  );
  expect([...declared].sort()).toEqual([...answered].sort() as string[]);
}

describe("transcript replay", () => {
  async function threeTurns(f: Fixture, offPath = true): Promise<Turn[]> {
    f.mock.setScript([
      ...toolThenAnswer("call-1", "answer one"),
      ...toolThenAnswer("call-2", "answer two"),
      { steps: [{ kind: "text", content: "answer three" }] },
    ]);
    // A steer queued before turn 1 is placed after its tool batch.
    await f.inputs.createInput({
      inputId: "steer-1",
      sessionId: f.chatId,
      kind: "steer",
      content: { content: "steer text" },
      contentHash: "h",
    });
    const one = await turn(f, "q1");
    const two = await turn(f, "q2", offPath ? one : undefined);
    const three = await turn(f, "q3", offPath ? two : undefined);
    return [one, two, three];
  }

  it("replays prior tool calls, results and steering before each final answer", async () => {
    const f = await setup();
    await threeTurns(f);
    const last = f.mock.requests.at(-1) ?? [];
    expect(shape(last)).toEqual([
      "user:q1",
      "assistant:calls(call-1)",
      "tool:call-1",
      "user:steer text",
      "assistant:answer one",
      "user:q2",
      "assistant:calls(call-2)",
      "tool:call-2",
      "assistant:answer two",
      "user:q3",
    ]);
  });

  it("replays each record once when the turns follow the active path", async () => {
    const f = await setup();
    await threeTurns(f, false);
    expect(shape(f.mock.requests.at(-1) ?? [])).toEqual([
      "user:q1",
      "assistant:calls(call-1)",
      "tool:call-1",
      "user:steer text",
      "assistant:answer one",
      "user:q2",
      "assistant:calls(call-2)",
      "tool:call-2",
      "assistant:answer two",
      "user:q3",
    ]);
  });

  it("gives a non-session chat today's final-answers-only history", async () => {
    const f = await setup({ session: false });
    await threeTurns(f);
    const last = f.mock.requests.at(-1) ?? [];
    expect(last).toEqual([
      { role: "user", content: "q1" },
      { role: "assistant", content: "answer one" },
      { role: "user", content: "q2" },
      { role: "assistant", content: "answer two" },
      { role: "user", content: "q3" },
    ]);
  });

  it("keeps final answers only when the descriptor says so", async () => {
    const f = await setup({
      session: { context: { policy: "final_answers" } },
    });
    await threeTurns(f);
    expect(shape(f.mock.requests.at(-1) ?? [])).toEqual([
      "user:q1",
      "assistant:answer one",
      "user:q2",
      "assistant:answer two",
      "user:q3",
    ]);
  });
});

describe("token budget", () => {
  const big = (label: string) => `${label} ${"x".repeat(2400)}`;

  it("drops the oldest whole turns, warns, and never splits a call/result pair", async () => {
    const f = await setup({
      session: { context: { windowTokens: 4000, reserveTokens: 1000 } },
    });
    f.mock.setScript([
      ...[1, 2, 3, 4, 5, 6].flatMap((n) =>
        toolThenAnswer(`call-${n}`, `answer ${n}`),
      ),
      { steps: [{ kind: "text", content: "final" }] },
    ]);
    let prev: Turn | undefined;
    for (const n of [1, 2, 3, 4, 5, 6])
      prev = await turn(f, big(`q${n}`), prev);
    const { runId } = await turn(f, "now", prev);
    const request = f.mock.requests.at(-1) ?? [];
    const labels = shape(request);
    expect(labels).not.toContain(`user:${big("q1").slice(0, 24)}`);
    expect(labels.at(-1)).toBe("user:now");
    expect(request[0]?.role).toBe("user");
    expectBalanced(request);
    const trimmed = (await warnings(f, runId)).find(
      (w) => w.code === "context_trimmed",
    );
    expect(trimmed?.message).toMatch(/Dropped the \d+ oldest turn/);
  });

  it("refuses an over-limit current turn without calling the provider", async () => {
    const f = await setup({
      session: { context: { windowTokens: 4000, reserveTokens: 1000 } },
    });
    const { runId } = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "y".repeat(40_000),
    });
    await expect(drive(f, runId)).rejects.toMatchObject({
      code: "context_overflow",
    });
    expect(f.mock.requests).toHaveLength(0);
    const events = (await f.store.tasks.listEvents(runId)) as AiRunEvent[];
    const failed = events.find((e) => e.type === "run.failed");
    expect(failed?.type === "run.failed" && failed.data.errorCode).toBe(
      "context_overflow",
    );
    expect((await f.store.tasks.getTask(runId))?.status).toBe("failed");
  });

  it("warns once when no window is known, and not when one is", async () => {
    const f = await setup();
    f.mock.setScript([{ steps: [{ kind: "text", content: "ok" }] }]);
    const { runId } = await turn(f, "hi");
    const fallback = (await warnings(f, runId)).filter(
      (w) => w.code === "context_budget_fallback",
    );
    expect(fallback).toHaveLength(1);

    const known = await setup({
      models: [{ modelId: "m1", contextWindowTokens: 100_000 }],
    });
    known.mock.setScript([{ steps: [{ kind: "text", content: "ok" }] }]);
    const { runId: knownRun } = await turn(known, "hi");
    expect(
      (await warnings(known, knownRun)).some(
        (w) => w.code === "context_budget_fallback",
      ),
    ).toBe(false);
  });

  it("does not warn or budget a non-session chat", async () => {
    const f = await setup({ session: false });
    f.mock.setScript([{ steps: [{ kind: "text", content: "ok" }] }]);
    const { runId } = await turn(f, "hi");
    expect(await warnings(f, runId)).toEqual([]);
  });

  it("resolves the window: descriptor, model, capability, fallback", () => {
    const capabilities = { maxContextTokens: 8000 };
    expect(
      resolveContextWindow({
        descriptor: { context: { windowTokens: 4000 } },
        model: { contextWindowTokens: 100_000 },
        capabilities,
      }),
    ).toEqual({ windowTokens: 4000, source: "descriptor" });
    expect(
      resolveContextWindow({
        descriptor: undefined,
        model: { contextWindowTokens: 100_000 },
        capabilities,
      }),
    ).toEqual({ windowTokens: 100_000, source: "model" });
    expect(
      resolveContextWindow({
        descriptor: undefined,
        model: undefined,
        capabilities,
      }),
    ).toEqual({ windowTokens: 8000, source: "capabilities" });
    expect(
      resolveContextWindow({
        descriptor: undefined,
        model: undefined,
        capabilities: null,
      }),
    ).toEqual({ windowTokens: 32768, source: "fallback" });
  });

  it("treats a steer after a tool result as part of its turn", () => {
    const call = { id: "c", name: "echo", argumentsJson: "{}" };
    const messages: AiChatMessage[] = [
      { role: "system", content: "s" },
      { role: "user", content: "x".repeat(400) },
      { role: "assistant", content: "", toolCalls: [call] },
      { role: "tool", content: "r", toolCallId: "c" },
      { role: "user", content: "steer" },
      { role: "assistant", content: "done" },
      { role: "user", content: "next" },
    ];
    const fit = fitToBudget(messages, {
      windowTokens: 100,
      reserveTokens: 0,
      toolTokens: 0,
      estimator: defaultTokenEstimator,
    });
    expect(fit.droppedTurns).toBe(1);
    expect(shape(fit.messages)).toEqual(["system:s", "user:next"]);
    expect(fit.overflow).toBe(false);
  });
});

describe("project instructions", () => {
  function source(files: Record<string, string>): ProjectInstructionsSource {
    return {
      async read(_root, name, maxBytes) {
        const text = files[name];
        if (text === undefined) return undefined;
        return text.length > maxBytes
          ? { text: text.slice(0, maxBytes), truncated: true }
          : { text };
      },
    };
  }

  it("injects AGENTS.md and CLAUDE.md with a sha label into the system prompt", async () => {
    const f = await setup({
      session: { workspace: { root: "/repo", trusted: true } },
      overrides: {
        projectInstructions: source({
          "AGENTS.md": "use bun",
          "CLAUDE.md": "be brief",
        }),
      },
    });
    f.mock.setScript([{ steps: [{ kind: "text", content: "ok" }] }]);
    await turn(f, "hi");
    const system = f.mock.requests[0]?.[0];
    expect(system?.role).toBe("system");
    const text = String(system?.content);
    expect(text).toMatch(
      /## Project instructions \(AGENTS\.md, sha256:[0-9a-f]{12}\)\nuse bun/,
    );
    expect(text).toMatch(
      /## Project instructions \(CLAUDE\.md, sha256:[0-9a-f]{12}\)\nbe brief/,
    );
    expect(text).not.toContain("untrusted");
  });

  it("marks untrusted instructions as never a permission grant", async () => {
    const f = await setup({
      session: { workspace: { root: "/repo", trusted: false } },
      overrides: { projectInstructions: source({ "AGENTS.md": "rules" }) },
    });
    f.mock.setScript([{ steps: [{ kind: "text", content: "ok" }] }]);
    await turn(f, "hi");
    const text = String(f.mock.requests[0]?.[0]?.content);
    expect(
      text.startsWith("The following project instructions come from untrusted"),
    ).toBe(true);
    expect(text).toContain("not a permission grant");
    expect(text).toContain("rules");
  });

  it("skips untrusted instructions when configured, labels them otherwise", async () => {
    const skip = await setup({
      session: { workspace: { root: "/repo", trusted: false } },
      overrides: {
        projectInstructions: source({ "AGENTS.md": "rules" }),
        untrustedProjectInstructions: "skip",
      },
    });
    skip.mock.setScript([{ steps: [{ kind: "text", content: "ok" }] }]);
    await turn(skip, "hi");
    const skipped = String(skip.mock.requests[0]?.[0]?.content ?? "");
    expect(skipped).not.toContain("rules");
    expect(skipped).not.toContain("untrusted");

    const trusted = await setup({
      session: { workspace: { root: "/repo", trusted: true } },
      overrides: {
        projectInstructions: source({ "AGENTS.md": "rules" }),
        untrustedProjectInstructions: "skip",
      },
    });
    trusted.mock.setScript([{ steps: [{ kind: "text", content: "ok" }] }]);
    await turn(trusted, "hi");
    expect(String(trusted.mock.requests[0]?.[0]?.content)).toContain("rules");

    const label = await setup({
      session: { workspace: { root: "/repo", trusted: false } },
      overrides: {
        projectInstructions: source({ "AGENTS.md": "rules" }),
        untrustedProjectInstructions: "label",
      },
    });
    label.mock.setScript([{ steps: [{ kind: "text", content: "ok" }] }]);
    await turn(label, "hi");
    const labelled = String(label.mock.requests[0]?.[0]?.content);
    expect(labelled).toContain("untrusted");
    expect(labelled).toContain("rules");
  });

  it("warns when a file was truncated", async () => {
    const f = await setup({
      session: { workspace: { root: "/repo", trusted: true } },
      overrides: {
        projectInstructions: {
          read: async () => ({ text: "cut", truncated: true }),
        },
      },
    });
    f.mock.setScript([{ steps: [{ kind: "text", content: "ok" }] }]);
    const { runId } = await turn(f, "hi");
    expect(
      (await warnings(f, runId)).some(
        (w) => w.code === "project_instructions_unavailable",
      ),
    ).toBe(true);
  });
});

describe("session skills", () => {
  const skillText =
    "---\nname: review-project\ndescription: Review\n---\nInspect the repository carefully.";

  async function withSkills() {
    const skills = new SkillService([
      bundledSkillSource("app", { review: { "SKILL.md": skillText } }),
    ]);
    const [entry] = await skills.catalog();
    const runContent = createRunContentProvider({
      templates: new PromptTemplateService(new MemoryPromptTemplateStore()),
      skills,
      authorizedConnectors: async () => [],
    });
    return { entry: entry!, runContent };
  }

  async function sessionFixture(
    skills: { id: string; revision: string }[],
    runContent: ReturnType<typeof createRunContentProvider>,
  ) {
    const f = await setup({ session: false, overrides: { runContent } });
    const taskRunner = new FakeTaskRunner();
    const service = new AgentSessionService({
      store: f.store,
      turns: f.runner,
      tasks: new TaskService({
        store: f.store,
        taskRunner,
        ids: f.ids,
        clock: f.clock,
      }),
      taskRunner,
      clock: f.clock,
      ids: f.ids,
    });
    await service.createSession({
      sessionId: "sess",
      profile: { id: "default" },
      providerId: "p1",
      model: "m1",
      skills,
    });
    return { f, service };
  }

  it("selects the descriptor's skills on every session turn", async () => {
    const { entry, runContent } = await withSkills();
    const { f, service } = await sessionFixture(
      [{ id: entry.id, revision: entry.revision }],
      runContent,
    );
    f.mock.setScript([{ steps: [{ kind: "text", content: "ok" }] }]);
    const sent = await service.send("sess", { content: "hi" });
    if (sent.kind !== "turn") throw new Error("expected a turn");
    f.chatId = "sess";
    await drive(f, sent.turnId);
    const system = String(f.mock.requests[0]?.[0]?.content);
    expect(system).toContain("Skill: review-project");
    expect(system).toContain("Inspect the repository carefully.");
  });

  it("warns and continues when a carried-over skill is gone", async () => {
    const { entry, runContent } = await withSkills();
    const { f, service } = await sessionFixture(
      [
        { id: entry.id, revision: entry.revision },
        { id: "app:missing", revision: "nope" },
      ],
      runContent,
    );
    f.mock.setScript([{ steps: [{ kind: "text", content: "ok" }] }]);
    const sent = await service.send("sess", { content: "hi" });
    if (sent.kind !== "turn") throw new Error("expected a turn");
    await drive(f, sent.turnId);
    const unavailable = (await warnings(f, sent.turnId)).filter(
      (w) => w.code === "skill_unavailable",
    );
    expect(unavailable).toHaveLength(1);
    expect(unavailable[0]?.message).toContain("app:missing");
    expect((await f.store.tasks.getTask(sent.turnId))?.status).toBe(
      "completed",
    );
    expect(String(f.mock.requests[0]?.[0]?.content)).toContain(
      "Inspect the repository carefully.",
    );
  });
});
