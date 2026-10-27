/**
 * Agent profiles (ADR 0015 §profiles): the same runtime configured for
 * different jobs, with the parts that matter enforced by the host.
 */
import { describe, expect, it } from "bun:test";
import type { AiChatMessage, AiRunEvent } from "@agentkit/contracts";
import type { AiChatRequest, AiProviderClient, AiTool } from "@agentkit/core";
import { MockProviderClient, type MockTurn } from "@agentkit/testing";
import {
  BUILT_IN_PROFILES,
  CODING_TOOL_NAMES,
  SESSION_METADATA_KEY,
  TurnRunner,
  createProfilePermissionAuthority,
  createProfileRegistry,
  defaultProfileRegistry,
  effectiveLimits,
  validateProfile,
  type AgentProfile,
  type AssistantStore,
  type ToolSetContributor,
} from "../src/index.js";
import { FakeAgentInputStore, createHarness } from "./fakes.js";

function tool(name: string, effect: "read" | "write" = "read"): AiTool {
  return {
    definition: {
      name,
      version: "1",
      effect,
      capability: `cap.${name}`,
      description: `the ${name} tool, described at length`,
      inputSchema: { type: "object", properties: {} },
    },
    async execute() {
      return {
        ok: true,
        data: {},
        sources: [],
        warnings: [],
        truncated: false,
        limits: { profile: "small", maxBytes: 1024 },
      };
    },
  } as unknown as AiTool;
}

const ALL = [
  tool("read_file"),
  tool("list_dir"),
  tool("code_search"),
  tool("repo_map"),
  tool("find_files"),
  tool("git_inspect"),
  tool("edit_file", "write"),
  tool("write_file", "write"),
  tool("run_command", "write"),
  tool("run_tests", "write"),
  tool("submit_plan"),
];

function contributor(tools: readonly AiTool[]): ToolSetContributor {
  return { namespace: "coding", contribute: async () => [...tools] };
}

async function stageNames(
  profile: AgentProfile | undefined,
  tools: readonly AiTool[] = ALL,
): Promise<{ staged: string[]; pruned: string[] }> {
  const { stageRegistry } = await import("../src/index.js");
  const staged = await stageRegistry({
    contributors: [contributor(tools)],
    ctx: { bindings: [], limits: { profile: "small", maxBytes: 1024 } },
    hasPrimaryBinding: true,
    ...(profile === undefined
      ? {}
      : { allowedToolNames: new Set(profile.tools) }),
  });
  return {
    staged: staged.registry.list().map((t) => t.definition.name),
    pruned: [...staged.pruned],
  };
}

describe("built-in profiles", () => {
  const registry = defaultProfileRegistry();

  it("covers the four jobs the runtime must serve", () => {
    for (const id of [
      "chat",
      "coding",
      "coding/plan",
      "research",
      "automation",
    ])
      expect(registry.resolve(id), id).toBeDefined();
    expect(registry.resolve("nope")).toBeUndefined();
  });

  it("a chat agent has no tools at all", () => {
    expect(registry.resolve("chat")?.tools).toEqual([]);
    expect(registry.resolve("chat")?.permissionMode).toBe("read_only");
  });

  it("the coding roles are ceilings over tools, mode and model", () => {
    const supervisor = registry.resolve("coding/supervisor");
    const reviewer = registry.resolve("coding/reviewer");
    const implementer = registry.resolve("coding");
    expect(supervisor?.role).toBe("supervisor");
    expect(reviewer?.role).toBe("reviewer");
    expect(implementer?.role).toBe("implementer");
    // Supervisor: reads only, and a mode that cannot escalate.
    expect(supervisor?.tools).toEqual(
      expect.arrayContaining(["read_file", "code_search", "git_inspect"]),
    );
    expect(supervisor?.tools.some((n) => /edit|write|run_/.test(n))).toBe(
      false,
    );
    expect(supervisor?.permissionMode).toBe("read_only");
    // Reviewer: may run tests, may not edit.
    expect(reviewer?.tools).toContain("run_tests");
    expect(reviewer?.tools).not.toContain("edit_file");
    expect(reviewer?.tools).not.toContain("write_file");
    // Implementer: everything, and it starts by asking.
    expect(implementer?.tools).toEqual(CODING_TOOL_NAMES);
    expect(implementer?.permissionMode).toBe("ask");
    // Independent limits per role.
    expect(supervisor?.limits.maxToolIterations).not.toBe(
      implementer?.limits.maxToolIterations,
    );
  });

  it("plan mode is a profile, not a flag the caller sets ad hoc", () => {
    expect(registry.resolve("coding/plan")?.permissionMode).toBe("plan");
    expect(registry.resolve("coding/plan")?.tools).toContain("submit_plan");
  });

  it("every built-in validates and stages its own tools", async () => {
    for (const profile of BUILT_IN_PROFILES) {
      expect(() => validateProfile(profile)).not.toThrow();
      const { staged } = await stageNames(profile);
      // Everything the profile lists must exist in the pack: a profile naming a
      // tool nobody contributes is a typo that silently does nothing.
      for (const name of profile.tools) expect(staged).toContain(name);
      if (profile.tools.length === 0) expect(staged).toEqual([]);
    }
  });
});

describe("profile enforcement", () => {
  it("stages exactly the allowlist and reports what it removed", async () => {
    const registry = createProfileRegistry([
      {
        id: "reader",
        kind: "research",
        instructions: "read only",
        tools: ["read_file", "code_search"],
        permissionMode: "read_only",
        limits: {},
        resources: {},
      },
    ]);
    const { staged, pruned } = await stageNames(registry.resolve("reader"));
    expect(staged.sort()).toEqual(["code_search", "read_file"]);
    expect(pruned).toContain("edit_file");
    expect(pruned).toContain("run_command");
  });

  it("an empty allowlist stages nothing at all", async () => {
    const { staged } = await stageNames(
      createProfileRegistry([
        {
          id: "blunt",
          kind: "chat",
          instructions: "",
          tools: [],
          permissionMode: "read_only",
          limits: {},
          resources: {},
        },
      ]).resolve("blunt"),
    );
    expect(staged).toEqual([]);
  });

  it("without a profile, staging is unchanged", async () => {
    const { staged, pruned } = await stageNames(undefined);
    expect(staged).toHaveLength(ALL.length);
    expect(pruned).toEqual([]);
  });

  it("a profile that lists a tool nobody contributes is reported, not hidden", async () => {
    const { pruned } = await stageNames(
      createProfileRegistry([
        {
          id: "wide",
          kind: "automation",
          instructions: "",
          tools: ["read_file", "deploy_to_prod"],
          permissionMode: "ask",
          limits: {},
          resources: {},
        },
      ]).resolve("wide"),
      [tool("read_file")],
    );
    expect(pruned).toEqual([]); // nothing was removed: it was never offered
  });

  it("refuses a profile that would widen a role ceiling", () => {
    const base: AgentProfile = {
      id: "bad",
      kind: "coding",
      instructions: "",
      permissionMode: "ask",
      limits: {},
      resources: {},
      tools: ["read_file"],
    };
    expect(() =>
      validateProfile({
        ...base,
        role: "supervisor",
        tools: ["read_file", "edit_file"],
      }),
    ).toThrow(/supervisor/);
    expect(() =>
      validateProfile({
        ...base,
        role: "reviewer",
        tools: ["read_file", "write_file"],
      }),
    ).toThrow(/reviewer/);
    expect(() =>
      validateProfile({ ...base, limits: { maxToolIterations: 0 } }),
    ).toThrow(/positive/);
    expect(() => validateProfile({ ...base, tools: ["a", "a"] })).toThrow(
      /duplicate/,
    );
  });
});

describe("profile limits", () => {
  it("a profile may lower a ceiling, never raise it", () => {
    const profile: AgentProfile = {
      id: "x",
      kind: "coding",
      instructions: "",
      tools: [],
      permissionMode: "ask",
      limits: { maxToolIterations: 500, historyLimit: 10 },
      resources: {},
    };
    const limits = effectiveLimits(profile, {
      maxToolIterations: 30,
      historyLimit: 200,
    });
    expect(limits.maxToolIterations).toBe(30);
    expect(limits.historyLimit).toBe(10);
  });

  it("absent on either side stays absent", () => {
    expect(effectiveLimits(undefined, {})).toEqual({
      maxToolIterations: undefined,
      maxToolCallsPerIteration: undefined,
      historyLimit: undefined,
      attemptBudgetMs: undefined,
      maxOutputTokens: undefined,
    });
  });
});

describe("the profile permission authority", () => {
  function storeFor(metadata: Record<string, unknown>): AssistantStore {
    return {
      conversations: {
        async getChat() {
          return { id: "s1", metadata: structuredClone(metadata) };
        },
      },
    } as unknown as AssistantStore;
  }

  const descriptor = (id: string): Record<string, unknown> => ({
    "agentkit.session": { version: 1, profile: { id }, createdAt: "t" },
  });

  it("the session's profile decides mode and role", async () => {
    const authority = createProfilePermissionAuthority({
      store: storeFor(descriptor("coding/reviewer")),
      profiles: defaultProfileRegistry(),
    });
    expect(await authority.modeOf({ chatId: "s1" })).toBe("ask");
    expect(await authority.roleOf({ chatId: "s1" })).toBe("reviewer");
  });

  it("a durable grant outranks the profile, and the profile outranks the fallback", async () => {
    const metadata = descriptor("coding/plan");
    const authority = createProfilePermissionAuthority({
      store: storeFor(metadata),
      profiles: defaultProfileRegistry(),
      fallbackMode: "full_auto",
      fallbackRole: "implementer",
    });
    expect(await authority.modeOf({ chatId: "s1" })).toBe("plan");
    metadata["agentkit.session.execution"] = {
      version: 1,
      mode: "auto_edit",
      updatedAt: "t",
      history: [],
      grantedPlan: {
        mode: "auto_edit",
        planDigest: "abc",
        requestId: "r",
        turnId: "t",
        approvedAt: "t",
      },
    };
    expect(await authority.modeOf({ chatId: "s1" })).toBe("auto_edit");
    expect(
      await authority.planApproved({ chatId: "s1", planDigest: "abc" }),
    ).toBe(true);
    expect(
      await authority.planApproved({ chatId: "s1", planDigest: "nope" }),
    ).toBe(false);
    // No session at all: the host's fallback answers, never the model.
    const bare = createProfilePermissionAuthority({
      store: storeFor({}),
      profiles: defaultProfileRegistry(),
      fallbackMode: "full_auto",
      fallbackRole: "implementer",
    });
    expect(await bare.modeOf({ chatId: "s1" })).toBe("full_auto");
    expect(await bare.roleOf({ chatId: "s1" })).toBe("implementer");
  });

  it("a session with an unknown profile falls back", async () => {
    const authority = createProfilePermissionAuthority({
      store: storeFor(descriptor("not-a-profile")),
      profiles: defaultProfileRegistry(),
      fallbackMode: "ask",
    });
    expect(await authority.modeOf({ chatId: "s1" })).toBe("ask");
    expect(await authority.roleOf({ chatId: "s1" })).toBeUndefined();
  });
});

describe("profile enforcement in a real turn", () => {
  /** Records the tool names each provider call was allowed to see. */
  class RecordingClient implements AiProviderClient {
    readonly id = "test";
    readonly kind = "openai-compatible" as const;
    readonly toolNamesPerCall: string[][] = [];
    readonly messagesPerCall: AiChatMessage[][] = [];
    constructor(readonly inner: AiProviderClient) {}
    async capabilities() {
      return this.inner.capabilities();
    }
    async listModels() {
      return this.inner.listModels();
    }
    async *streamChat(input: AiChatRequest): AsyncIterable<AiRunEvent> {
      this.toolNamesPerCall.push((input.tools ?? []).map((t) => t.name));
      this.messagesPerCall.push([...input.messages]);
      yield* this.inner.streamChat(input);
    }
  }

  async function setup(profileId: string, script: MockTurn[]) {
    const harness = createHarness();
    harness.store.inputs = new FakeAgentInputStore(harness.clock);
    await harness.store.providers.upsertProvider({
      id: "p1",
      label: "Mock",
      kind: "openai-compatible",
      baseUrl: "http://localhost:1234",
      defaultModel: "m1",
      enabled: true,
    });
    await harness.store.settings.updateSettings({ defaultProviderId: "p1" });
    const mock = new MockProviderClient();
    mock.setScript(script);
    const client = new RecordingClient(mock);
    let mutated = "";
    const mutating = ["edit_file", "write_file", "run_command", "run_tests"];
    const writes: AiTool[] = mutating.map((name) => ({
      definition: {
        name,
        version: "1",
        effect: "write",
        capability: `cap.${name}`,
        description: `${name} does the thing`,
        inputSchema: { type: "object", properties: {} },
      },
      async execute() {
        mutated = name;
        return {
          ok: true,
          data: {},
          sources: [],
          warnings: [],
          truncated: false,
          limits: { profile: "small", maxBytes: 1024 },
        };
      },
    }));
    const runner = new TurnRunner({
      store: harness.store,
      taskRunner: harness.taskRunner,
      providerFactory: () => client,
      contributors: [
        contributor([
          ...ALL.filter((t) => !mutating.includes(t.definition.name)),
          ...writes,
        ]),
      ],
      profiles: defaultProfileRegistry(),
      clock: harness.clock,
      ids: harness.ids,
    });
    const chat = await harness.store.conversations.createChat({
      id: "s1",
      metadata: {
        [SESSION_METADATA_KEY]: {
          version: 1,
          profile: { id: profileId },
          createdAt: "2026-01-01T00:00:00.000Z",
        },
      },
    });
    const submitted = await runner.submitMessage({
      chatId: chat.id,
      content: "make the change",
      model: "m1",
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
    await runner.execute({
      taskId: submitted.runId,
      attemptId: attempt.attemptId,
      leaseToken: lease.leaseToken,
      signal: new AbortController().signal,
    });
    const events = (await harness.store.tasks.listEvents(
      submitted.runId,
    )) as AiRunEvent[];
    return { harness, client, events, mutated, submitted };
  }

  const callEdit = (toolCallId = "c1"): MockTurn => ({
    steps: [
      {
        kind: "tool_call",
        toolCallId,
        name: "edit_file",
        argumentsJson: "{}",
      },
    ],
  });
  const answer = (content: string): MockTurn => ({
    steps: [{ kind: "text", content }],
  });

  it("the model is never shown a tool the profile removed", async () => {
    const f = await setup("coding/supervisor", [callEdit(), answer("done")]);
    expect(f.client.toolNamesPerCall[0]).not.toContain("edit_file");
    expect(f.client.toolNamesPerCall[0]).toContain("read_file");
    // Asking anyway is a tool_missing failure, not a mutation.
    const failed = f.events.find((e) => e.type === "run.tool.failed") as
      | { data: { errorCode?: string; toolName?: string } }
      | undefined;
    expect(failed?.data.errorCode).toBe("tool_missing");
    expect(f.mutated).toBe("");
    // And the decision is on the record, distinguishable from a contributor
    // that failed.
    expect(
      f.events.some(
        (e) =>
          e.type === "run.warning" &&
          (e.data as { code?: string }).code === "profile_tools_pruned",
      ),
    ).toBe(true);
  });

  it("the same runtime stages the write tools for the implementer profile", async () => {
    const f = await setup("coding", [callEdit(), answer("done")]);
    expect(f.client.toolNamesPerCall[0]).toContain("edit_file");
    expect(f.mutated).toBe("edit_file");
  });

  it("the profile's own instructions reach the system prompt", async () => {
    const f = await setup("coding/plan", [answer("planned")]);
    const system = f.client.messagesPerCall[0]?.find(
      (m) => m.role === "system",
    );
    expect(system?.content).toContain(
      "Agent profile: Coding agent (plan first)",
    );
    expect(system?.content).toContain("wait for approval");
  });
});
