/**
 * Shared drivers for the golden run-event scenarios, used by both
 * `golden.test.ts` (live replay against the current run-loop) and
 * `scripts/record-goldens.ts` (regenerating the committed traces).
 *
 * `finishReason:"incomplete"` is deliberately NOT one of these scenarios: it
 * is produced by the SSE provider client, not the run-loop, and is already
 * covered by `openai-compatible-assembly.test.ts`.
 *
 * This file lives under `tests/`, not `src/`, on purpose: it imports
 * `@agentkit/core` at runtime to drive `runChat`, and
 * `packages/testing/src/**` must stay core-runtime-free (see this package's
 * README — `@agentkit/core`/`@agentkit/host` are peer dependencies used only
 * in this package's own tests, never imported at runtime by `src/`, so that
 * `@agentkit/core`'s tests can import mocks from `@agentkit/testing` without
 * a circular runtime dependency). `tests/` is where the package's own tests
 * are already allowed to import `@agentkit/core` (see `golden.test.ts`,
 * `hanging-provider.test.ts`).
 */
import {
  runChat,
  AiToolRegistry,
  resolveToolLimits,
  createEventStamper,
  nowIso,
} from "@agentkit/core";
import type { AiTool, AiProviderClient, AiChatRequest } from "@agentkit/core";
import type { AiRunEvent } from "@agentkit/contracts";
import { MockProviderClient } from "../src/mock-provider.js";
import { makeUserMessage } from "../src/fixtures.js";
import type { GoldenTraceName } from "../src/golden/golden.js";

/** Drain a `runChat` generator down to just its yielded events. */
async function collect(
  gen: AsyncGenerator<AiRunEvent, unknown, unknown>,
): Promise<AiRunEvent[]> {
  const events: AiRunEvent[] = [];
  for (;;) {
    const next = await gen.next();
    if (next.done) return events;
    events.push(next.value);
  }
}

function makeEchoTool(): AiTool<{ text: string }, { echoed: string }> {
  return {
    definition: {
      name: "echo",
      version: "1",
      effect: "read",
      capability: "test",
      description: "Echoes its input back.",
      inputSchema: {
        type: "object",
        properties: { text: { type: "string" } },
        required: ["text"],
      },
    },
    async execute(ctx, input) {
      return {
        ok: true,
        data: { echoed: input.text },
        sources: [],
        warnings: [],
        truncated: false,
        limits: ctx.limits,
      };
    },
  };
}

/**
 * A minimal AiProviderClient that emits a couple of events and then throws —
 * the shape of a socket reset or a malformed response body. Built inline
 * (rather than in `@agentkit/testing`'s mocks) because a throwing client is
 * scenario-specific recording/replay tooling, not a reusable test double.
 */
class ThrowingProviderClient implements AiProviderClient {
  readonly id = "golden-throwing";
  readonly kind = "openai-compatible";
  async capabilities() {
    return { streaming: true, toolCalling: true, modelList: false };
  }
  async listModels() {
    return [];
  }
  async *streamChat(input: AiChatRequest): AsyncIterable<AiRunEvent> {
    const stamp = createEventStamper();
    yield stamp({
      type: "run.started",
      runId: input.runId,
      timestamp: nowIso(),
      data: { model: input.model, toolCount: 0 },
    });
    yield stamp({
      type: "run.message.delta",
      runId: input.runId,
      timestamp: nowIso(),
      data: { delta: "half an ans" },
    });
    throw new Error("socket hang up");
  }
}

const MODEL = "gpt-golden";
const LIMITS = resolveToolLimits({ preference: "small" });

async function recordChatOnly(): Promise<AiRunEvent[]> {
  const client = new MockProviderClient();
  client.setScript([
    {
      steps: [
        { kind: "text", content: "Hello from the golden chat-only trace." },
      ],
    },
  ]);
  return collect(
    runChat({
      client,
      registry: new AiToolRegistry(),
      model: MODEL,
      messages: [makeUserMessage("hi")],
      limits: LIMITS,
      runId: "run-golden-chat-only",
      firstSeq: 0,
    }),
  );
}

async function recordToolRun(): Promise<AiRunEvent[]> {
  const client = new MockProviderClient();
  client.setScript([
    {
      steps: [
        {
          kind: "tool_call",
          toolCallId: "call-golden-1",
          name: "echo",
          argumentsJson: '{"text":"golden"}',
        },
      ],
    },
    { steps: [{ kind: "text", content: "Done." }] },
  ]);
  const registry = new AiToolRegistry();
  registry.register(makeEchoTool() as unknown as AiTool);
  return collect(
    runChat({
      client,
      registry,
      model: MODEL,
      messages: [makeUserMessage("please echo golden")],
      limits: LIMITS,
      runId: "run-golden-tool-run",
      firstSeq: 0,
    }),
  );
}

async function recordCancelledRun(): Promise<AiRunEvent[]> {
  const client = new MockProviderClient();
  client.setScript([{ steps: [{ kind: "text", content: "never sent" }] }]);
  const controller = new AbortController();
  controller.abort();
  return collect(
    runChat({
      client,
      registry: new AiToolRegistry(),
      model: MODEL,
      messages: [makeUserMessage("go")],
      limits: LIMITS,
      runId: "run-golden-cancelled-run",
      firstSeq: 0,
      signal: controller.signal,
    }),
  );
}

async function recordFailedRun(): Promise<AiRunEvent[]> {
  return collect(
    runChat({
      client: new ThrowingProviderClient(),
      registry: new AiToolRegistry(),
      model: MODEL,
      messages: [makeUserMessage("hi")],
      limits: LIMITS,
      runId: "run-golden-failed-run",
      firstSeq: 0,
    }),
  );
}

async function recordUsageRun(): Promise<AiRunEvent[]> {
  const client = new MockProviderClient();
  client.emitUsage = true;
  client.setScript([
    { steps: [{ kind: "text", content: "Usage-tracked answer." }] },
  ]);
  return collect(
    runChat({
      client,
      registry: new AiToolRegistry(),
      model: MODEL,
      messages: [makeUserMessage("hi")],
      limits: LIMITS,
      runId: "run-golden-usage-run",
      firstSeq: 0,
    }),
  );
}

async function recordToolCapRun(): Promise<AiRunEvent[]> {
  const client = new MockProviderClient();
  client.setScript([
    {
      steps: [
        {
          kind: "tool_call",
          toolCallId: "call-golden-cap-1",
          name: "echo",
          argumentsJson: '{"text":"a"}',
        },
        {
          kind: "tool_call",
          toolCallId: "call-golden-cap-2",
          name: "echo",
          argumentsJson: '{"text":"b"}',
        },
      ],
    },
    { steps: [{ kind: "text", content: "Done." }] },
  ]);
  const registry = new AiToolRegistry();
  registry.register(makeEchoTool() as unknown as AiTool);
  return collect(
    runChat({
      client,
      registry,
      model: MODEL,
      messages: [makeUserMessage("please echo twice")],
      limits: LIMITS,
      runId: "run-golden-tool-cap-run",
      firstSeq: 0,
      maxToolCallsPerIteration: 1,
    }),
  );
}

async function recordDuplicateIdRun(): Promise<AiRunEvent[]> {
  const client = new MockProviderClient();
  client.setScript([
    {
      steps: [
        {
          kind: "tool_call",
          toolCallId: "call-golden-dup",
          name: "echo",
          argumentsJson: '{"text":"one"}',
        },
        {
          kind: "tool_call",
          toolCallId: "call-golden-dup",
          name: "echo",
          argumentsJson: '{"text":"two"}',
        },
      ],
    },
    { steps: [{ kind: "text", content: "Done." }] },
  ]);
  const registry = new AiToolRegistry();
  registry.register(makeEchoTool() as unknown as AiTool);
  return collect(
    runChat({
      client,
      registry,
      model: MODEL,
      messages: [makeUserMessage("please echo twice")],
      limits: LIMITS,
      runId: "run-golden-duplicate-id-run",
      firstSeq: 0,
    }),
  );
}

/**
 * A tool whose `execute` never resolves, the shape of a remote tool behind a
 * hung connection. `defaultToolTimeoutMs` is fixed and small so the run
 * terminates quickly and deterministically — the emitted error message
 * embeds the configured timeout, not the actual elapsed wall-clock time, so
 * the recorded trace is stable across replays.
 */
function makeHangingTool(): AiTool<Record<string, never>, unknown> {
  return {
    definition: {
      name: "hangs",
      version: "1",
      effect: "read",
      capability: "test",
      description: "Never resolves.",
      inputSchema: { type: "object", properties: {} },
    },
    execute: () => new Promise(() => {}),
  };
}

async function recordToolTimeoutRun(): Promise<AiRunEvent[]> {
  const client = new MockProviderClient();
  client.setScript([
    {
      steps: [
        {
          kind: "tool_call",
          toolCallId: "call-golden-timeout",
          name: "hangs",
          argumentsJson: "{}",
        },
      ],
    },
    { steps: [{ kind: "text", content: "Done." }] },
  ]);
  const registry = new AiToolRegistry();
  registry.register(makeHangingTool() as unknown as AiTool);
  return collect(
    runChat({
      client,
      registry,
      model: MODEL,
      messages: [makeUserMessage("please hang")],
      limits: LIMITS,
      runId: "run-golden-tool-timeout-run",
      firstSeq: 0,
      defaultToolTimeoutMs: 5,
    }),
  );
}

/**
 * A tool returning a BigInt in its `data` — not JSON-serializable, the shape
 * of a counter read straight off a driver.
 */
function makeUnserializableTool(): AiTool<Record<string, never>, unknown> {
  return {
    definition: {
      name: "counter",
      version: "1",
      effect: "read",
      capability: "test",
      description: "Returns a BigInt count.",
      inputSchema: { type: "object", properties: {} },
    },
    async execute(ctx) {
      return {
        ok: true,
        data: { count: BigInt(9007199254740993n) },
        sources: [],
        warnings: [],
        truncated: false,
        limits: ctx.limits,
      };
    },
  };
}

async function recordUnserializableRun(): Promise<AiRunEvent[]> {
  const client = new MockProviderClient();
  client.setScript([
    {
      steps: [
        {
          kind: "tool_call",
          toolCallId: "call-golden-unserializable",
          name: "counter",
          argumentsJson: "{}",
        },
      ],
    },
    { steps: [{ kind: "text", content: "Done." }] },
  ]);
  const registry = new AiToolRegistry();
  registry.register(makeUnserializableTool() as unknown as AiTool);
  return collect(
    runChat({
      client,
      registry,
      model: MODEL,
      messages: [makeUserMessage("please count")],
      limits: LIMITS,
      runId: "run-golden-unserializable-run",
      firstSeq: 0,
    }),
  );
}

/**
 * One driver per golden scenario, keyed by {@link GoldenTraceName}. Both
 * `scripts/record-goldens.ts` (writes the committed trace) and
 * `golden.test.ts` (replays live and compares against the committed trace)
 * run these same functions, so there is exactly one place that knows how to
 * drive each scenario.
 */
export const GOLDEN_SCENARIOS: Record<
  GoldenTraceName,
  () => Promise<AiRunEvent[]>
> = {
  "chat-only": recordChatOnly,
  "tool-run": recordToolRun,
  "cancelled-run": recordCancelledRun,
  "failed-run": recordFailedRun,
  "usage-run": recordUsageRun,
  "tool-cap-run": recordToolCapRun,
  "duplicate-id-run": recordDuplicateIdRun,
  "tool-timeout-run": recordToolTimeoutRun,
  "unserializable-run": recordUnserializableRun,
};
