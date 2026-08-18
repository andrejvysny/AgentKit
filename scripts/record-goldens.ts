#!/usr/bin/env bun
/**
 * Regenerates the golden run-event traces committed at
 * `packages/testing/src/golden/traces/*.json`.
 *
 * `packages/testing/tests/golden.test.ts` and
 * `packages/contracts/tests/golden-validate.test.ts` check the committed
 * trace files. They are NOT regenerated on every test run — the timestamps
 * and event ids inside them are frozen history, exactly as recorded here, and
 * are committed verbatim (see `normalizeTrace` in
 * `packages/testing/src/golden/golden.ts`, which is what makes comparing a
 * live run against a frozen trace meaningful despite that).
 *
 * Regenerate deliberately, when a scenario or the run-loop's event shape
 * changes on purpose — not routinely:
 *
 *   bun scripts/record-goldens.ts
 *
 * Then run `bun test` and diff the resulting trace files before committing; a
 * golden trace changing is a signal to look at, not something to rubber-stamp.
 *
 * Imports below are relative into each package's `src/`, not the
 * `@agentkit/*` package names — this script is repo tooling, not package
 * code, and it is the one place in this diff allowed to touch `@agentkit/core`
 * at runtime (`packages/testing/src/**` must stay core-runtime-free; see that
 * package's README/CLAUDE.md note on why `@agentkit/core` is a peer
 * dependency there).
 */
import { mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import {
  runChat,
  AiToolRegistry,
  resolveToolLimits,
  createEventStamper,
  nowIso,
} from "../packages/core/src/index.js";
import type {
  AiTool,
  AiProviderClient,
  AiChatRequest,
} from "../packages/core/src/index.js";
import { MockProviderClient } from "../packages/testing/src/mock-provider.js";
import { makeUserMessage } from "../packages/testing/src/fixtures.js";
import type { AiRunEvent } from "../packages/contracts/src/index.js";

const TRACES_DIR = join(
  import.meta.dir,
  "..",
  "packages",
  "testing",
  "src",
  "golden",
  "traces",
);

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
 * (rather than in `@agentkit/testing`) because a throwing client is
 * scenario-specific recording tooling, not a reusable test double.
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

const SCENARIOS: Record<string, () => Promise<AiRunEvent[]>> = {
  "chat-only": recordChatOnly,
  "tool-run": recordToolRun,
  "cancelled-run": recordCancelledRun,
  "failed-run": recordFailedRun,
  "usage-run": recordUsageRun,
};

async function main(): Promise<void> {
  mkdirSync(TRACES_DIR, { recursive: true });
  for (const [name, record] of Object.entries(SCENARIOS)) {
    const events = await record();
    if (events.length === 0) {
      throw new Error(`Scenario "${name}" recorded zero events.`);
    }
    const path = join(TRACES_DIR, `${name}.json`);
    writeFileSync(path, `${JSON.stringify(events, null, 2)}\n`, "utf8");
    console.log(`wrote ${path} (${events.length} events)`);
  }
}

main().catch((err: unknown) => {
  console.error(err);
  process.exit(1);
});
