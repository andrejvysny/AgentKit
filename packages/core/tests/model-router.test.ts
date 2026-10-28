import { describe, expect, it } from "bun:test";
import type {
  AiChatMessage,
  AiProviderCapabilities,
  AiProviderModel,
  AiRunEvent,
  AiToolResult,
} from "@agentkit/contracts";
import { CONTRACT_VERSION } from "@agentkit/contracts";
import {
  AiToolRegistry,
  ModelRoutingError,
  createCapabilityAwareRouter,
  resolveToolLimits,
  runAgentLoop,
  type AiChatRequest,
  type AiProviderClient,
  type AiTool,
  type ModelRef,
  type ModelRouter,
  type ModelSelectionContext,
} from "../src/index.js";

const limits = resolveToolLimits({ preference: "small" });

function ev(type: string, data: unknown): AiRunEvent {
  return {
    type: type as AiRunEvent["type"],
    runId: "x",
    timestamp: "2026-01-01T00:00:00.000Z",
    contractVersion: CONTRACT_VERSION,
    seq: 0,
    data,
  } as AiRunEvent;
}

class Scripted implements AiProviderClient {
  readonly kind = "openai-compatible" as const;
  readonly calls: AiChatRequest[] = [];
  constructor(
    readonly id: string,
    private readonly script: AiRunEvent[][],
  ) {}
  async capabilities() {
    return { streaming: true, toolCalling: true, modelList: true };
  }
  async listModels(): Promise<AiProviderModel[]> {
    return [];
  }
  async *streamChat(input: AiChatRequest): AsyncIterable<AiRunEvent> {
    const turn = this.script[this.calls.length];
    if (!turn) throw new Error(`script exhausted on ${this.id}`);
    this.calls.push(input);
    for (const event of turn) yield event;
  }
}

class Untouched implements AiProviderClient {
  readonly kind = "openai-compatible" as const;
  touched = false;
  constructor(readonly id: string) {}
  private fail(): never {
    this.touched = true;
    throw new Error(`unauthorized provider ${this.id} was called`);
  }
  async capabilities(): Promise<AiProviderCapabilities> {
    return this.fail();
  }
  listModels(): Promise<AiProviderModel[]> {
    return this.fail();
  }
  streamChat(): AsyncIterable<AiRunEvent> {
    return this.fail();
  }
}

function textTurn(model: string, content: string): AiRunEvent[] {
  return [
    ev("run.message.requested", {}),
    ev("run.message.delta", { delta: content }),
    ev("run.message.completed", { content, finishReason: "stop" }),
    ev("run.usage", {
      callId: `call-${model}`,
      attempt: 1,
      step: 1,
      model,
      promptTokens: 10,
      completionTokens: 5,
      totalTokens: 15,
      source: "response",
      finalForCall: true,
    }),
  ];
}

function toolTurn(calls: { id: string; name: string }[]): AiRunEvent[] {
  return [
    ev("run.message.requested", {}),
    ...calls.map((c) =>
      ev("run.tool.requested", {
        toolCallId: c.id,
        toolName: c.name,
        argumentsJson: "{}",
      }),
    ),
    ev("run.message.completed", {
      content: "",
      finishReason: "tool_calls",
      toolCallCount: calls.length,
    }),
  ];
}

function grepTool(execs: string[]): AiTool {
  return {
    definition: {
      name: "grep",
      version: "1.0.0",
      description: "grep",
      capability: "test",
      effect: "read",
      inputSchema: { type: "object", additionalProperties: true },
    },
    execute: async (): Promise<AiToolResult> => {
      execs.push("grep");
      return {
        ok: true,
        data: "match",
        sources: [],
        warnings: [],
        truncated: false,
        limits,
      };
    },
  };
}

const messages: AiChatMessage[] = [
  { role: "user", content: "do the thing" } as AiChatMessage,
];

async function run(
  it: AsyncGenerator<AiRunEvent, { terminal: string }, unknown>,
): Promise<{ events: AiRunEvent[]; terminal: string }> {
  const events: AiRunEvent[] = [];
  let result = await it.next();
  while (!result.done) {
    events.push(result.value);
    result = await it.next();
  }
  return { events, terminal: result.value.terminal };
}

function registryWith(tools: AiTool[]): AiToolRegistry {
  const registry = new AiToolRegistry();
  for (const t of tools) registry.register(t);
  return registry;
}

function stepRouter(steps: (ModelRef | undefined)[]): ModelRouter {
  let i = 0;
  return {
    selectModel: (ctx) => {
      const sel = steps[i];
      i += 1;
      return sel ?? ctx.previousModel ?? { providerId: "pA", modelId: "m1" };
    },
  };
}

describe("model routing (B3)", () => {
  it("switches providers per step, executes the tool once, attributes usage", async () => {
    const execs: string[] = [];
    const pA = new Scripted("pA", [toolTurn([{ id: "t1", name: "grep" }])]);
    const pB = new Scripted("pB", [textTurn("m2", "final answer")]);
    const pC = new Untouched("pC");
    const router = stepRouter([undefined, { providerId: "pB", modelId: "m2" }]);
    const { events, terminal } = await run(
      runAgentLoop({
        client: pA,
        model: "m1",
        registry: registryWith([grepTool(execs)]),
        messages,
        limits,
        requiredCapabilities: ["toolCalling"],
        modelRouter: router,
        resolveClient: async (providerId) => {
          if (providerId === "pB") return pB;
          if (providerId === "pC") return pC;
          throw new Error(`unknown provider ${providerId}`);
        },
      }),
    );
    expect(terminal).toBe("completed");
    expect(pA.calls.length).toBe(1);
    expect(pB.calls.length).toBe(1);
    expect(pB.calls[0]?.model).toBe("m2");
    expect(pC.touched).toBe(false);
    expect(execs).toEqual(["grep"]);
    // the routed provider's usage names its own model
    const usage = events.filter((e) => e.type === "run.usage");
    expect(usage.at(-1)?.data).toMatchObject({ model: "m2" });
    // exactly one terminal event, after everything else
    expect(events.at(-1)?.type).toBe("run.completed");
  });

  it("passes full selection context to the router", async () => {
    const pA = new Scripted("pA", [textTurn("m1", "done")]);
    const seen: ModelSelectionContext[] = [];
    await run(
      runAgentLoop({
        client: pA,
        model: "m1",
        registry: registryWith([]),
        messages,
        limits,
        requiredCapabilities: ["vision"],
        modelRouter: {
          selectModel: (ctx) => {
            seen.push(ctx);
            return { providerId: "pA", modelId: "m1" };
          },
        },
      }),
    );
    expect(seen.length).toBe(1);
    const ctx = seen[0] as ModelSelectionContext;
    expect(ctx.step).toBe(1);
    expect(ctx.requiredCapabilities).toEqual(["vision"]);
    expect(ctx.previousModel).toEqual({ providerId: "pA", modelId: "m1" });
    expect(typeof ctx.runId).toBe("string");
  });

  it("fails closed on a cross-provider switch without resolveClient", async () => {
    const pA = new Scripted("pA", [textTurn("m1", "never")]);
    const { events, terminal } = await run(
      runAgentLoop({
        client: pA,
        model: "m1",
        registry: registryWith([]),
        messages,
        limits,
        modelRouter: {
          selectModel: () => ({ providerId: "pB", modelId: "m2" }),
        },
      }),
    );
    expect(terminal).toBe("failed");
    expect(events.at(-1)).toMatchObject({
      type: "run.failed",
      data: { errorCode: "provider_unresolved" },
    });
    expect(pA.calls.length).toBe(0);
  });

  it("rejects a model switch while continuation exists", async () => {
    const pA = new Scripted("pA", [textTurn("m1", "never")]);
    const { events, terminal } = await run(
      runAgentLoop({
        client: pA,
        model: "m1",
        registry: registryWith([]),
        messages,
        limits,
        continuation: { opaque: "state" } as never,
        modelRouter: {
          selectModel: () => ({ providerId: "pA", modelId: "m2" }),
        },
      }),
    );
    expect(terminal).toBe("failed");
    expect(events.at(-1)).toMatchObject({
      type: "run.failed",
      data: { errorCode: "continuation_conflict" },
    });
    expect(pA.calls.length).toBe(0);
  });

  it("reset policy discards continuation with host authorization", async () => {
    const pB = new Scripted("pB", [textTurn("m2", "resumed elsewhere")]);
    const { terminal } = await run(
      runAgentLoop({
        client: new Scripted("pA", []),
        model: "m1",
        registry: registryWith([]),
        messages,
        limits,
        continuation: { opaque: "state" } as never,
        continuationModelSwitch: "reset",
        modelRouter: {
          selectModel: () => ({ providerId: "pB", modelId: "m2" }),
        },
        resolveClient: async () => pB,
      }),
    );
    expect(terminal).toBe("completed");
    expect(pB.calls.length).toBe(1);
    expect(pB.calls[0]?.continuation).toBeUndefined();
  });

  it("host refusal vetoes dispatch", async () => {
    const pB = new Scripted("pB", [textTurn("m2", "never")]);
    const { events, terminal } = await run(
      runAgentLoop({
        client: new Scripted("pA", []),
        model: "m1",
        registry: registryWith([]),
        messages,
        limits,
        modelRouter: {
          selectModel: () => ({ providerId: "pB", modelId: "m2" }),
        },
        resolveClient: async () => pB,
        authorizeModelSelection: (sel) => {
          if (sel.providerId !== "pA") throw new Error("policy: pB is banned");
        },
      }),
    );
    expect(terminal).toBe("failed");
    expect(events.at(-1)).toMatchObject({
      type: "run.failed",
      data: { errorCode: "authorization_denied" },
    });
    expect(pB.calls.length).toBe(0);
  });

  it("malformed router selections fail closed", async () => {
    const pA = new Scripted("pA", [textTurn("m1", "never")]);
    const { events, terminal } = await run(
      runAgentLoop({
        client: pA,
        model: "m1",
        registry: registryWith([]),
        messages,
        limits,
        modelRouter: {
          selectModel: () => ({ providerId: "", modelId: "m1" }),
        },
      }),
    );
    expect(terminal).toBe("failed");
    expect(events.at(-1)).toMatchObject({
      type: "run.failed",
      data: { errorCode: "invalid_selection" },
    });
    expect(pA.calls.length).toBe(0);
  });

  it("routed requests still consume the provider-request budget", async () => {
    const pA = new Scripted("pA", [toolTurn([{ id: "t1", name: "grep" }])]);
    const pB = new Scripted("pB", [textTurn("m2", "never")]);
    const execs: string[] = [];
    const { events, terminal } = await run(
      runAgentLoop({
        client: pA,
        model: "m1",
        registry: registryWith([grepTool(execs)]),
        messages,
        limits,
        budgets: { providerRequests: 1 },
        modelRouter: (() => {
          let n = 0;
          return {
            selectModel: () =>
              n++ === 0
                ? { providerId: "pA", modelId: "m1" }
                : { providerId: "pB", modelId: "m2" },
          } satisfies ModelRouter;
        })(),
        resolveClient: async () => pB,
      }),
    );
    // step 1 consumes the single provider request; the routed step 2 is
    // refused by the budget, not dispatched.
    expect(terminal).toBe("failed");
    expect(pB.calls.length).toBe(0);
    expect(events.at(-1)?.type).toBe("run.failed");
  });

  it("a sticky router never re-resolves clients", async () => {
    const pA = new Scripted("pA", [textTurn("m1", "done")]);
    let resolves = 0;
    const { terminal } = await run(
      runAgentLoop({
        client: pA,
        model: "m1",
        registry: registryWith([]),
        messages,
        limits,
        modelRouter: {
          selectModel: (ctx) =>
            ctx.previousModel ?? { providerId: "pA", modelId: "m1" },
        },
        resolveClient: async () => {
          resolves += 1;
          return pA;
        },
      }),
    );
    expect(terminal).toBe("completed");
    expect(resolves).toBe(0);
    expect(pA.calls.length).toBe(1);
  });
});

const catalog: AiProviderModel[] = [
  {
    providerId: "pA",
    modelId: "m-small",
    displayName: null,
    supportsToolCalling: false,
    fetchedAt: "2026-01-01T00:00:00.000Z",
  },
  {
    providerId: "pA",
    modelId: "m-big",
    displayName: null,
    supportsToolCalling: true,
    supportsVision: true,
    fetchedAt: "2026-01-01T00:00:00.000Z",
    pricing: {
      currency: "USD",
      inputPerMillionTokens: 5,
      priceSource: "host",
    },
  },
  {
    providerId: "pB",
    modelId: "m-cheap",
    displayName: null,
    supportsToolCalling: true,
    fetchedAt: "2026-01-01T00:00:00.000Z",
    pricing: {
      currency: "USD",
      inputPerMillionTokens: 0.5,
      priceSource: "host",
    },
  },
];

const baseCtx = (
  over: Partial<ModelSelectionContext> = {},
): ModelSelectionContext => ({
  runId: "r1",
  step: 1,
  requiredCapabilities: [],
  ...over,
});

describe("createCapabilityAwareRouter (B3)", () => {
  it("picks a model that declares every required capability", async () => {
    const router = createCapabilityAwareRouter({ models: () => catalog });
    const sel = await router.selectModel(
      baseCtx({ requiredCapabilities: ["toolCalling"] }),
    );
    expect(sel.modelId).toBe("m-big"); // first qualifying in catalogue order
  });

  it("keeps the previous model while it still qualifies", async () => {
    const router = createCapabilityAwareRouter({ models: () => catalog });
    const sel = await router.selectModel(
      baseCtx({
        requiredCapabilities: ["toolCalling"],
        previousModel: { providerId: "pB", modelId: "m-cheap" },
      }),
    );
    expect(sel).toEqual({ providerId: "pB", modelId: "m-cheap" });
  });

  it("unknown capabilities can never be satisfied — no silent fallback", async () => {
    const router = createCapabilityAwareRouter({ models: () => catalog });
    await expect(
      router.selectModel(baseCtx({ requiredCapabilities: ["telepathy"] })),
    ).rejects.toMatchObject({ code: "no_capable_model" });
  });

  it("absent capability facts do not count as satisfied", async () => {
    const router = createCapabilityAwareRouter({ models: () => catalog });
    await expect(
      router.selectModel(baseCtx({ requiredCapabilities: ["vision"] })),
    ).resolves.toEqual({ providerId: "pA", modelId: "m-big" });
  });

  it("prefer ranks qualifying models (cost-aware)", async () => {
    const router = createCapabilityAwareRouter({
      models: () => catalog,
      prefer: (m) => -(m.pricing?.inputPerMillionTokens ?? Number.MAX_VALUE),
    });
    const sel = await router.selectModel(
      baseCtx({ requiredCapabilities: ["toolCalling"] }),
    );
    expect(sel).toEqual({ providerId: "pB", modelId: "m-cheap" });
  });

  it("provider filter restricts the candidate pool", async () => {
    const router = createCapabilityAwareRouter({
      models: () => catalog,
      providerId: "pA",
    });
    await expect(
      router.selectModel(baseCtx({ requiredCapabilities: ["toolCalling"] })),
    ).resolves.toEqual({ providerId: "pA", modelId: "m-big" });
  });

  it("selectStepModel surfaces ModelRoutingError codes", async () => {
    const { selectStepModel } = await import("../src/runs/model-router.js");
    await expect(
      selectStepModel({
        router: { selectModel: () => ({ providerId: "x", modelId: "" }) },
        current: { providerId: "pA", modelId: "m1" },
        runId: "r",
        step: 1,
        requiredCapabilities: [],
        hasContinuation: false,
        continuationSwitchPolicy: "reject",
      }),
    ).rejects.toBeInstanceOf(ModelRoutingError);
  });
});
