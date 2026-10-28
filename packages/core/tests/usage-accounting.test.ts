import { describe, expect, it } from "bun:test";
import type {
  AiChatMessage,
  AiProviderModel,
  AiRunEvent,
} from "@agentkit/contracts";
import { CONTRACT_VERSION } from "@agentkit/contracts";
import {
  AiToolRegistry,
  createRunTelemetryHooks,
  emitRunTelemetry,
  estimateRunCost,
  resolveToolLimits,
  runAgentLoop,
  summarizeRunUsage,
  type RunTelemetryEvent,
  type RunTelemetrySink,
} from "../src/index.js";

const limits = resolveToolLimits({ preference: "small" });

function ev(
  type: string,
  data: unknown,
  at = "2026-01-01T00:00:00.000Z",
): AiRunEvent {
  return {
    type: type as AiRunEvent["type"],
    runId: "run-1",
    timestamp: at,
    contractVersion: CONTRACT_VERSION,
    seq: 0,
    data,
  } as AiRunEvent;
}

const usage = (
  callId: string,
  model: string,
  over: Record<string, unknown> = {},
): AiRunEvent =>
  ev("run.usage", {
    callId,
    attempt: 1,
    step: 1,
    model,
    promptTokens: 100,
    completionTokens: 20,
    source: "response",
    finalForCall: true,
    ...over,
  });

describe("summarizeRunUsage (B4)", () => {
  it("counts a call once: the final wins over stream drafts", () => {
    const summary = summarizeRunUsage([
      usage("c1", "m", {
        promptTokens: 40,
        completionTokens: 5,
        source: "stream",
        finalForCall: false,
      }),
      usage("c1", "m", { promptTokens: 100, completionTokens: 20 }),
    ]);
    expect(summary.providerRequests).toBe(1);
    expect(summary.models[0]?.inputTokens).toBe(100);
    expect(summary.models[0]?.outputTokens).toBe(20);
  });

  it("a duplicate final for the same call never double-counts", () => {
    const summary = summarizeRunUsage([
      usage("c1", "m", { promptTokens: 100 }),
      usage("c1", "m", { promptTokens: 100 }),
    ]);
    expect(summary.providerRequests).toBe(1);
    expect(summary.models[0]?.inputTokens).toBe(100);
  });

  it("a cut-short call with no final still bills once", () => {
    const summary = summarizeRunUsage([
      usage("c1", "m", { finalForCall: false, source: "stream" }),
    ]);
    expect(summary.providerRequests).toBe(1);
    expect(summary.providerCallsCutShort).toBe(1);
    expect(summary.models[0]?.inputTokens).toBe(100);
  });

  it("retries are counted separately from billed requests", () => {
    const summary = summarizeRunUsage([
      usage("c1", "m", { finalForCall: false }),
      ev("run.warning", { code: "provider_retry", message: "503" }),
      usage("c2", "m"),
    ]);
    expect(summary.providerRequests).toBe(2);
    expect(summary.providerRetries).toBe(1);
    // the failed attempt really was billed — its tokens stay counted
    expect(summary.models[0]?.inputTokens).toBe(200);
  });

  it("same callId, different attempts: each attempt is billed (C3)", () => {
    // model-step re-stamps one provider step's attempts under a shared
    // callId with attempt 1..n; (callId, attempt) dedup must NOT collapse
    // them — every attempt was a real, billed provider request.
    const summary = summarizeRunUsage([
      usage("c1", "m", { attempt: 1 }),
      usage("c1", "m", { attempt: 2 }),
    ]);
    expect(summary.providerRequests).toBe(2);
    expect(summary.models[0]?.requests).toBe(2);
    expect(summary.models[0]?.inputTokens).toBe(200);
    expect(summary.models[0]?.outputTokens).toBe(40);
  });

  it("draft/final pairs dedup per (callId, attempt), never across attempts", () => {
    const summary = summarizeRunUsage([
      // attempt 1: draft then final — only the final bills.
      usage("c1", "m", {
        attempt: 1,
        promptTokens: 30,
        completionTokens: 5,
        source: "stream",
        finalForCall: false,
      }),
      usage("c1", "m", {
        attempt: 1,
        promptTokens: 100,
        completionTokens: 20,
        source: "response",
        finalForCall: true,
      }),
      // attempt 2: cut-short draft — stands in for its own key, billed once.
      usage("c1", "m", {
        attempt: 2,
        promptTokens: 70,
        completionTokens: 10,
        source: "stream",
        finalForCall: false,
      }),
    ]);
    expect(summary.providerRequests).toBe(2);
    expect(summary.models[0]?.requests).toBe(2);
    // attempt 1 bills its final (100/20), attempt 2 its draft (70/10)
    expect(summary.models[0]?.inputTokens).toBe(170);
    expect(summary.models[0]?.outputTokens).toBe(30);
    expect(summary.providerCallsCutShort).toBe(1);
  });

  it("usage events without a callId never collapse into one bucket", () => {
    // A missing (or empty) callId is not a shared identity: each such event
    // is its own provider request, never one merged singleton.
    const summary = summarizeRunUsage([
      usage("", "m", { callId: "" }),
      ev("run.usage", {
        attempt: 1,
        step: 1,
        model: "m",
        promptTokens: 100,
        completionTokens: 20,
        source: "response",
        finalForCall: true,
      }),
    ]);
    expect(summary.providerRequests).toBe(2);
    expect(summary.models[0]?.requests).toBe(2);
    expect(summary.models[0]?.inputTokens).toBe(200);
    expect(summary.models[0]?.outputTokens).toBe(40);
  });

  it("attributes usage per provider and model", () => {
    const summary = summarizeRunUsage([
      usage("c1", "m1", { providerId: "pB" }),
      usage("c2", "m2", { providerId: "pA" }),
      usage("c3", "m1", { providerId: "pA" }),
      usage("c4", "m1", { providerId: "pA" }),
    ]);
    expect(summary.models.map((m) => `${m.providerId}/${m.model}`)).toEqual([
      "pA/m1",
      "pA/m2",
      "pB/m1",
    ]);
    expect(summary.models[0]?.requests).toBe(2);
  });

  it("unattributed usage lands under the unknown provider", () => {
    const summary = summarizeRunUsage([usage("c1", "m")]);
    expect(summary.models[0]?.providerId).toBe("unknown");
  });

  it("derives durations from event timestamps", () => {
    const base = Date.parse("2026-01-01T00:00:00.000Z");
    const at = (ms: number) => new Date(base + ms).toISOString();
    const summary = summarizeRunUsage([
      ev("run.started", { model: "m", toolCount: 1 }, at(0)),
      ev("run.message.delta", { delta: "hi" }, at(150)),
      ev(
        "run.waiting",
        {
          requestId: "hreq:t:c1",
          toolCallId: "t1",
          toolName: "edit_file",
          step: 1,
          prompt: "approve?",
          kind: "approval",
        },
        at(200),
      ),
      ev(
        "run.tool.running",
        { toolCallId: "t1", toolName: "edit_file" },
        at(700),
      ),
      ev(
        "run.tool.succeeded",
        { toolCallId: "t1", toolName: "edit_file", sources: [] },
        at(1200),
      ),
      ev("run.completed", { iterations: 1 }, at(1300)),
    ]);
    expect(summary.timeToFirstTokenMs).toBe(150);
    expect(summary.durationMs).toBe(1300);
    expect(summary.approvals).toBe(1);
    expect(summary.approvalWaitMs).toBe(500); // 200 -> 700
    expect(summary.toolExecutionMs).toBe(500); // 700 -> 1200
    expect(summary.toolExecutions).toBe(1);
  });

  it("counts compaction and input waits", () => {
    const summary = summarizeRunUsage([
      ev("run.context.compacted", {
        summaryId: "s1",
        tokensBefore: 9000,
        tokensAfter: 1500,
        coveredMessages: 12,
      }),
      ev("run.waiting", {
        requestId: "hreq:t:c1",
        toolCallId: "t1",
        toolName: "ask",
        step: 1,
        prompt: "which?",
      }),
      ev("run.completed", { iterations: 1 }),
    ]);
    expect(summary.compactions).toBe(1);
    expect(summary.compactionTokensSaved).toBe(7500);
    expect(summary.inputWaits).toBe(1);
    expect(summary.approvals).toBe(0);
  });

  it("is deterministic: the same events always fold to the same summary", () => {
    const events = [
      ev("run.started", { model: "m", toolCount: 0 }),
      usage("c1", "b-model", { providerId: "pB" }),
      usage("c2", "a-model", { providerId: "pA" }),
      ev("run.completed", { iterations: 1 }),
    ];
    expect(summarizeRunUsage(events)).toEqual(summarizeRunUsage(events));
    expect(JSON.stringify(summarizeRunUsage(events))).toBe(
      JSON.stringify(summarizeRunUsage(events)),
    );
  });
});

const pricedA: AiProviderModel = {
  providerId: "pA",
  modelId: "m-fast",
  displayName: null,
  fetchedAt: "2026-01-01T00:00:00.000Z",
  pricing: {
    currency: "USD",
    inputPerMillionTokens: 2,
    outputPerMillionTokens: 8,
    cachedInputPerMillionTokens: 0.5,
    priceSource: "host",
    effectiveDate: "2026-10-01",
  },
};

describe("estimateRunCost (B4)", () => {
  it("prices input, cached input and output separately", () => {
    const summary = summarizeRunUsage([
      usage("c1", "m-fast", {
        providerId: "pA",
        promptTokens: 1_000_000,
        cachedPromptTokens: 400_000,
        completionTokens: 100_000,
      }),
    ]);
    const cost = estimateRunCost(summary, [pricedA]);
    expect(cost.status).toBe("priced");
    const entry = cost.entries[0];
    // 600k uncached @ $2/M + 400k cached @ $0.5/M + 100k out @ $8/M
    expect(entry?.inputCost).toBeCloseTo(1.2);
    expect(entry?.cachedInputCost).toBeCloseTo(0.2);
    expect(entry?.outputCost).toBeCloseTo(0.8);
    expect(entry?.amount).toBeCloseTo(2.2);
    expect(entry?.currency).toBe("USD");
    expect(entry?.priceSource).toBe("host");
    expect(entry?.effectiveDate).toBe("2026-10-01");
    expect(cost.totals.USD).toBeCloseTo(2.2);
  });

  it("unknown pricing is never zero-filled", () => {
    const summary = summarizeRunUsage([
      usage("c1", "m-mystery", { providerId: "pZ" }),
    ]);
    const cost = estimateRunCost(summary, [pricedA]);
    expect(cost.status).toBe("unknown");
    expect(cost.entries[0]?.amount).toBeUndefined();
    expect(cost.entries[0]?.unknownReasons).toContain(
      "no pricing for this model",
    );
    expect(cost.totals).toEqual({});
  });

  it("partial pricing marks the estimate partial", () => {
    const summary = summarizeRunUsage([
      usage("c1", "m-fast", { providerId: "pA" }),
      usage("c2", "m-mystery", { providerId: "pZ" }),
    ]);
    const cost = estimateRunCost(summary, [pricedA]);
    expect(cost.status).toBe("partial");
    expect(cost.entries[0]?.amount).toBeDefined();
    expect(cost.entries[1]?.amount).toBeUndefined();
  });

  it("a future effectiveDate is not priced relative to `at` (C3)", () => {
    // pricedA takes effect 2026-10-01; on 2026-09-15 that price is
    // published-but-not-live: the entry stays unpriced and says why.
    const summary = summarizeRunUsage([
      usage("c1", "m-fast", { providerId: "pA" }),
    ]);
    const cost = estimateRunCost(summary, [pricedA], "2026-09-15");
    expect(cost.status).toBe("unknown");
    expect(cost.entries[0]?.amount).toBeUndefined();
    expect(cost.entries[0]?.inputCost).toBeUndefined();
    expect(cost.entries[0]?.outputCost).toBeUndefined();
    expect(cost.entries[0]?.unknownReasons).toContain(
      "pricing not yet effective",
    );
    expect(cost.totals).toEqual({});
  });

  it("a past or exactly-current effectiveDate prices normally at `at` (C3)", () => {
    const summary = summarizeRunUsage([
      usage("c1", "m-fast", {
        providerId: "pA",
        promptTokens: 1_000_000,
        completionTokens: 0,
      }),
    ]);
    // the effectiveDate itself counts as effective (not strictly future)
    const onDay = estimateRunCost(summary, [pricedA], "2026-10-01");
    expect(onDay.status).toBe("priced");
    expect(onDay.entries[0]?.amount).toBeCloseTo(2);
    // a Date instance after the effectiveDate prices as before
    const later = estimateRunCost(summary, [pricedA], new Date("2026-12-01"));
    expect(later.status).toBe("priced");
    expect(later.entries[0]?.amount).toBeCloseTo(2);
  });

  it("absent effectiveDate prices at any `at`, even in the past (C3)", () => {
    const undated: AiProviderModel = {
      ...pricedA,
      pricing: {
        currency: "USD",
        inputPerMillionTokens: 2,
        outputPerMillionTokens: 8,
        priceSource: "host",
      },
    };
    const summary = summarizeRunUsage([
      usage("c1", "m-fast", {
        providerId: "pA",
        promptTokens: 1_000_000,
        completionTokens: 0,
      }),
    ]);
    const cost = estimateRunCost(summary, [undated], "2020-01-01");
    expect(cost.status).toBe("priced");
    expect(cost.entries[0]?.amount).toBeCloseTo(2);
    expect(cost.entries[0]?.unknownReasons).toEqual([]);
  });

  it("missing cached rate bills cached as input and says so", () => {
    const noCachedRate: AiProviderModel = {
      ...pricedA,
      pricing: {
        currency: "USD",
        inputPerMillionTokens: 2,
        outputPerMillionTokens: 8,
        priceSource: "provider",
      },
    };
    const summary = summarizeRunUsage([
      usage("c1", "m-fast", {
        providerId: "pA",
        promptTokens: 1_000_000,
        cachedPromptTokens: 500_000,
        completionTokens: 0,
      }),
    ]);
    const cost = estimateRunCost(summary, [noCachedRate]);
    const entry = cost.entries[0];
    expect(entry?.cachedBilledAsInput).toBe(true);
    expect(entry?.cachedInputCost).toBeCloseTo(1.0);
    expect(entry?.inputCost).toBeCloseTo(1.0);
  });

  it("reasoning not billed as output is surfaced as unknown", () => {
    const noReasoningRate: AiProviderModel = {
      ...pricedA,
      pricing: {
        currency: "USD",
        inputPerMillionTokens: 2,
        outputPerMillionTokens: 8,
        reasoningTokensBilledAsOutput: false,
        priceSource: "preset",
      },
    };
    const summary = summarizeRunUsage([
      usage("c1", "m-fast", {
        providerId: "pA",
        completionTokens: 100,
        reasoningTokens: 50,
      }),
    ]);
    const cost = estimateRunCost(summary, [noReasoningRate]);
    // one model, insufficient data: the whole estimate is unknown, not zero
    expect(cost.status).toBe("unknown");
    expect(cost.entries[0]?.amount).toBeUndefined();
    expect(cost.entries[0]?.unknownReasons.join()).toContain(
      "reasoning tokens",
    );
  });

  it("totals are tracked per currency", () => {
    const pricedEur: AiProviderModel = {
      providerId: "pB",
      modelId: "m-eu",
      displayName: null,
      fetchedAt: "2026-01-01T00:00:00.000Z",
      pricing: {
        currency: "EUR",
        inputPerMillionTokens: 1,
        outputPerMillionTokens: 1,
        priceSource: "provider",
      },
    };
    const summary = summarizeRunUsage([
      usage("c1", "m-fast", {
        providerId: "pA",
        promptTokens: 1_000_000,
        completionTokens: 0,
      }),
      usage("c2", "m-eu", {
        providerId: "pB",
        promptTokens: 2_000_000,
        completionTokens: 0,
      }),
    ]);
    const cost = estimateRunCost(summary, [pricedA, pricedEur]);
    expect(cost.status).toBe("priced");
    expect(cost.totals.USD).toBeCloseTo(2.0);
    expect(cost.totals.EUR).toBeCloseTo(2.0);
  });
});

/* ------------------------------ observability ------------------------------ */

class RecordingSink implements RunTelemetrySink {
  events: RunTelemetryEvent[] = [];
  constructor(private readonly explode = false) {}
  emit(event: RunTelemetryEvent): void {
    if (this.explode) throw new Error("telemetry backend down");
    this.events.push(event);
  }
}

class ScriptedProvider {
  readonly id = "pA";
  readonly kind = "openai-compatible" as const;
  constructor(private readonly turns: AiRunEvent[][]) {}
  private calls = 0;
  async capabilities() {
    return { streaming: true, toolCalling: true, modelList: true };
  }
  async listModels(): Promise<AiProviderModel[]> {
    return [];
  }
  async *streamChat(input: { runId?: string }): AsyncIterable<AiRunEvent> {
    const turn = this.turns[this.calls] ?? [];
    this.calls += 1;
    for (const event of turn) yield { ...event, runId: input.runId ?? "run-1" };
  }
}

const SECRETPROMPT = "the-user-secret-prompt-text";

describe("run telemetry (B4)", () => {
  it("emits sanitized lifecycle telemetry during a run", async () => {
    const sink = new RecordingSink();
    const client = new ScriptedProvider([
      [
        ev("run.message.requested", {}),
        ev("run.message.delta", { delta: "ok" }),
        ev("run.message.completed", { content: "ok", finishReason: "stop" }),
        usage("c1", "m1", { providerId: "pA" }),
      ],
    ]);
    const messages: AiChatMessage[] = [
      { role: "user", content: SECRETPROMPT } as AiChatMessage,
    ];
    const registry = new AiToolRegistry();
    for await (const _event of runAgentLoop({
      client: client as never,
      model: "m1",
      registry,
      messages,
      limits,
      hooks: [createRunTelemetryHooks(sink)],
    })) {
      // drain
    }
    const names = sink.events.map((e) => e.name);
    expect(names).toContain("agentkit.run.start");
    expect(names).toContain("agentkit.step.start");
    expect(names).toContain("agentkit.run.settled");
    // prompts / user content never travel through telemetry
    const wire = JSON.stringify(sink.events);
    expect(wire.includes(SECRETPROMPT)).toBe(false);
    expect(wire.includes("delta")).toBe(false);
  });

  it("a throwing telemetry sink cannot fail the run", async () => {
    const errors: unknown[] = [];
    const client = new ScriptedProvider([
      [
        ev("run.message.requested", {}),
        ev("run.message.completed", { content: "ok", finishReason: "stop" }),
      ],
    ]);
    let terminal = "unset";
    for await (const event of runAgentLoop({
      client: client as never,
      model: "m1",
      registry: new AiToolRegistry(),
      messages: [{ role: "user", content: "hi" } as AiChatMessage],
      limits,
      hooks: [
        createRunTelemetryHooks(new RecordingSink(true), {
          onError: (e) => errors.push(e),
        }),
      ],
    })) {
      if (event.type === "run.completed") terminal = "completed";
    }
    expect(terminal).toBe("completed");
    expect(errors.length).toBeGreaterThan(0);
  });

  it("emitRunTelemetry carries accounting numbers, not content", () => {
    const sink = new RecordingSink();
    const summary = summarizeRunUsage([
      ev("run.started", { model: "m", toolCount: 0 }),
      usage("c1", "m", { providerId: "pA" }),
      ev("run.completed", { iterations: 1 }),
    ]);
    emitRunTelemetry(sink, summary, {
      extraAttributes: { tenant: "acme" },
    });
    const event = sink.events[0];
    expect(event?.name).toBe("agentkit.run.usage");
    expect(event?.attributes.providerRequests).toBe(1);
    expect(event?.attributes.tenant).toBe("acme");
    expect(JSON.stringify(event)).not.toContain("promptTokens");
  });
});
