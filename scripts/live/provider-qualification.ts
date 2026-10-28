/**
 * OPTIONAL live provider qualification (B5) against a local OpenAI-compatible
 * endpoint. NOT part of standard CI: it first probes the endpoint and exits 0
 * with a clear "absent, skipping" message when nothing is listening.
 *
 *   bun scripts/live/provider-qualification.ts
 *
 * Env:
 *   AGENTKIT_LIVE_BASE_URL  default http://127.0.0.1:8000/v1
 *   AGENTKIT_LIVE_MODEL     default Qwen3.8-Flash-Next
 *   AGENTKIT_LIVE_FIXTURE   fixture path; default an mkdtemp path (throwaway).
 *
 * What it does: records a small deterministic scenario set through
 * `@agentkit/cassette` (streamed text, streamed tool call, usage), then
 * replays the fixture offline and asserts the replayed streams match the
 * recorded ones. The fixture is metadata-capture by construction: prompts and
 * completions never land in it, so the default throwaway path is safe.
 */
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { AiRunEvent } from "../../packages/contracts/src/index.js";
import { OpenAiCompatibleClient } from "../../packages/core/src/index.js";
import { createCassette } from "../../packages/cassette/src/index.js";

const BASE_URL =
  process.env["AGENTKIT_LIVE_BASE_URL"] ?? "http://127.0.0.1:8000/v1";
const MODEL = process.env["AGENTKIT_LIVE_MODEL"] ?? "Qwen3.8-Flash-Next";

async function endpointAlive(): Promise<boolean> {
  try {
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), 1500);
    const response = await fetch(`${BASE_URL}/models`, {
      signal: controller.signal,
    });
    clearTimeout(timer);
    return response.ok;
  } catch {
    return false;
  }
}

if (!(await endpointAlive())) {
  console.log(
    `provider-qualification: no OpenAI-compatible endpoint at ${BASE_URL}; skipping (this is expected in standard CI).`,
  );
  process.exit(0);
}

const dir = await mkdtemp(join(tmpdir(), "ak-live-qual-"));
const fixturePath =
  process.env["AGENTKIT_LIVE_FIXTURE"] ?? join(dir, "qualification.json");

function makeClient(): OpenAiCompatibleClient {
  return new OpenAiCompatibleClient({
    id: "live",
    kind: "openai-compatible",
    baseUrl: BASE_URL,
  });
}

async function collect(
  iterable: AsyncIterable<AiRunEvent>,
): Promise<AiRunEvent[]> {
  const events: AiRunEvent[] = [];
  for await (const event of iterable) events.push(event);
  return events;
}

const scenarios: {
  name: string;
  request: () => Parameters<OpenAiCompatibleClient["streamChat"]>[0];
  assert: (events: AiRunEvent[]) => void;
}[] = [
  {
    name: "streamed text completes",
    request: () =>
      ({
        runId: "qual-text",
        model: MODEL,
        messages: [
          { role: "user", content: "Reply with exactly the word: present" },
        ],
      }) as never,
    assert: (events) => {
      const completed = events.find((e) => e.type === "run.message.completed");
      if (!completed) throw new Error("no run.message.completed");
    },
  },
  {
    name: "streamed tool call assembles",
    request: () =>
      ({
        runId: "qual-tool",
        model: MODEL,
        messages: [
          {
            role: "user",
            content:
              "Call the tool echo with {text: ping}. Only call the tool, do not answer.",
          },
        ],
        tools: [
          {
            name: "echo",
            version: "1",
            effect: "read",
            capability: "qual",
            description: "Echo text.",
            inputSchema: {
              type: "object",
              properties: { text: { type: "string" } },
              required: ["text"],
            },
          },
        ],
      }) as never,
    assert: (events) => {
      const requested = events.filter((e) => e.type === "run.tool.requested");
      if (requested.length === 0)
        throw new Error("model produced no tool call (acceptable flake)");
      for (const call of requested) {
        JSON.parse(
          (call.data as { argumentsJson: string }).argumentsJson ?? "",
        );
      }
    },
  },
  {
    name: "usage numbers arrive",
    request: () =>
      ({
        runId: "qual-usage",
        model: MODEL,
        messages: [{ role: "user", content: "Say: ok" }],
      }) as never,
    assert: (events) => {
      const usage = events.find((e) => e.type === "run.usage");
      if (!usage) throw new Error("provider reported no usage");
    },
  },
];

let failures = 0;
const record = createCassette({ mode: "record", fixture: fixturePath });
const recorded: Record<string, string[]> = {};
for (const scenario of scenarios) {
  try {
    const events = await collect(
      record.provider(makeClient()).streamChat(scenario.request()),
    );
    scenario.assert(events);
    recorded[scenario.name] = events.map((e) => e.type);
    console.log(`  record ok: ${scenario.name} (${events.length} events)`);
  } catch (error) {
    failures += 1;
    console.error(
      `  record FAIL: ${scenario.name}: ${error instanceof Error ? error.message : String(error)}`,
    );
  }
}
await record.close();

if (failures === 0) {
  const replay = createCassette({ mode: "replay", fixture: fixturePath });
  for (const scenario of scenarios) {
    try {
      const events = await collect(
        replay
          .provider(
            new OpenAiCompatibleClient({
              id: "live",
              kind: "openai-compatible",
              baseUrl: BASE_URL,
              // replay must never send a byte; a touch means a bug
              fetchImpl: (async () => {
                throw new Error("network touched during replay");
              }) as unknown as typeof fetch,
            }),
          )
          .streamChat(scenario.request()),
      );
      const types = events
        .map((e) => e.type)
        // metadata capture never records hidden reasoning text (security
        // design), so replayed streams legitimately drop reasoning deltas.
        .filter((t) => t !== "run.message.reasoning-delta");
      const want = (recorded[scenario.name] ?? []).filter(
        (t) => t !== "run.message.reasoning-delta",
      );
      const same =
        types.length === want.length && types.every((t, i) => t === want[i]);
      if (!same)
        throw new Error(
          `replay diverged: [${types.join(",")}] != [${want.join(",")}]`,
        );
      console.log(`  replay ok: ${scenario.name}`);
    } catch (error) {
      failures += 1;
      console.error(
        `  replay FAIL: ${scenario.name}: ${error instanceof Error ? error.message : String(error)}`,
      );
    }
  }
  await replay.close();
}

await rm(dir, { recursive: true, force: true });
console.log(
  failures === 0
    ? `provider-qualification: PASS against ${BASE_URL} (${MODEL})`
    : `provider-qualification: ${failures} failure(s)`,
);
process.exit(failures === 0 ? 0 : 1);
