import { describe, expect, it } from "bun:test";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { AiRunEvent } from "@agentkit/contracts";
import type { AiProviderClient, AiTool } from "@agentkit/core";
import { createCassette } from "@agentkit/cassette";
import { MockProviderClient, type MockTurn } from "@agentkit/testing";
import { TurnRunner } from "../src/index.js";
import { createHarness, type TestHarness } from "./fakes.js";

/**
 * Cassette × host integration (Phase B/C): the cassette attaches at the two
 * host-owned boundaries — `providerFactory` and tool contributors — with NO
 * host-side cassette awareness. A recorded turn must replay offline: same
 * durable event sequence, same visible assistant message, tool bodies never
 * executed, provider transport never touched.
 *
 * This is the release gate for "record a real coding session once, qualify
 * the runtime on it forever" — deterministic replay at the HOST level, not
 * just at the loop level.
 */

const BASE = { id: "p1", model: "m1" } as const;

const toolScript: MockTurn[] = [
  {
    steps: [
      {
        kind: "tool_call",
        toolCallId: "call-1",
        name: "read_note",
        argumentsJson: JSON.stringify({ note: "alpha" }),
      },
    ],
    finishReason: "tool_calls",
  },
  { steps: [{ kind: "text", content: "the note says alpha" }] },
];

function makeNoteTool(onExecute: () => void): AiTool {
  return {
    definition: {
      name: "read_note",
      version: "1",
      effect: "read",
      capability: "read",
      description: "read a note",
      inputSchema: { type: "object" },
    },
    async execute(ctx, input) {
      onExecute();
      const note = String((input as { note?: string }).note ?? "");
      return {
        ok: true,
        data: { text: `note:${note}` },
        sources: [],
        warnings: [],
        truncated: false,
        limits: ctx.limits,
      };
    },
  };
}

const tempDirs: string[] = [];
async function fixturePath(): Promise<string> {
  const dir = await mkdtemp(join(tmpdir(), "cassette-host-"));
  tempDirs.push(dir);
  return join(dir, "turn-fixture.json");
}

async function cleanup(): Promise<void> {
  for (const dir of tempDirs.splice(0))
    await rm(dir, { recursive: true, force: true });
}

interface TurnFixture extends TestHarness {
  runner: TurnRunner;
  chatId: string;
}

async function setup(
  clientFor: () => AiProviderClient,
  tool: AiTool,
): Promise<TurnFixture> {
  const harness = createHarness();
  await harness.store.providers.upsertProvider({
    id: BASE.id,
    label: "Base",
    kind: "openai-compatible",
    baseUrl: "http://localhost:1234",
    defaultModel: BASE.model,
    enabled: true,
  });
  await harness.store.settings.updateSettings({ defaultProviderId: BASE.id });
  const chat = await harness.store.conversations.createChat({ id: "chat-1" });
  const runner = new TurnRunner({
    store: harness.store,
    taskRunner: harness.taskRunner,
    providerFactory: () => clientFor(),
    contributors: [
      { namespace: "notes", contribute: async () => [tool] } as never,
    ],
    clock: harness.clock,
    ids: harness.ids,
  });
  return { ...harness, runner, chatId: chat.id };
}

async function driveTurn(f: TurnFixture): Promise<{
  taskId: string;
  events: AiRunEvent[];
  visible: string | undefined;
}> {
  const submitted = await f.runner.submitMessage({
    chatId: f.chatId,
    content: "what does the note say?",
    model: BASE.model,
  });
  const taskId = submitted.runId;
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
  const events = (await f.store.tasks.listEvents(taskId)) as AiRunEvent[];
  const messages = await f.store.conversations.listMessages(f.chatId);
  const assistant = messages.filter(
    (m) => m.role === "assistant" && m.metadata["internal"] !== true,
  );
  const last = assistant.at(-1)?.content;
  const visibleText =
    typeof last === "string"
      ? last
      : last?.map((part) => (part.type === "text" ? part.text : "")).join("");
  return {
    taskId,
    events,
    visible: visibleText,
  };
}

describe("cassette x host turn integration", () => {
  it("records a tool-using turn and replays it fully offline", async () => {
    const path = await fixturePath();
    try {
      // -- record: live provider + live tool body. The tool goes through
      // `wrapTool` at the contributor boundary — the host stays cassette-
      // unaware; the test's composition is the documented integration.
      // CONTENT capture: the host CONSUMES these events into conversation
      // history, so a host-level fixture must pin what the user sees. --
      const record = createCassette({
        mode: "record",
        fixture: path,
        capture: "content",
      });
      const live = new MockProviderClient();
      live.setScript(toolScript);
      live.emitUsage = true;
      let liveToolRuns = 0;
      const recording = await setup(
        () => record.provider(live),
        record.wrapTool(
          makeNoteTool(() => {
            liveToolRuns += 1;
          }),
        ),
      );
      const recorded = await driveTurn(recording);
      await record.close();

      expect(recorded.events.map((e) => e.type)).toContain("run.completed");
      expect(recorded.visible).toBe("the note says alpha");
      expect(liveToolRuns).toBe(1);
      expect(live.callCount).toBe(2);

      // -- replay: the provider client behind the wrapper is a script that
      // would answer differently; the fixture must win and the inner client
      // must never be invoked --
      const replay = createCassette({ mode: "replay", fixture: path });
      const untouched = new MockProviderClient();
      untouched.setScript([
        { steps: [{ kind: "text", content: "MUST NEVER BE SERVED" }] },
      ]);
      let replayToolRuns = 0;
      const replaying = await setup(
        () => replay.provider(untouched),
        replay.wrapTool(
          makeNoteTool(() => {
            replayToolRuns += 1;
            throw new Error("tool body executed during replay");
          }),
        ),
      );
      const replayed = await driveTurn(replaying);
      await replay.close();

      expect(replayToolRuns).toBe(0);
      expect(untouched.callCount).toBe(0);
      expect(replayed.events.map((e) => e.type)).toEqual(
        recorded.events.map((e) => e.type),
      );
      expect(replayed.visible).toBe(recorded.visible);
    } finally {
      await cleanup();
    }
  }, 30_000);

  it("verify passes a faithful re-execution and fails a divergent one", async () => {
    const path = await fixturePath();
    try {
      // Content capture: verify must pin TEXT equality, not just shape.
      const record = createCassette({
        mode: "record",
        fixture: path,
        capture: "content",
      });
      const live = new MockProviderClient();
      live.setScript(toolScript);
      const recording = await setup(
        () => record.provider(live),
        record.wrapTool(makeNoteTool(() => undefined)),
      );
      await driveTurn(recording);
      await record.close();

      // Faithful: same script, same tool behavior — executes live, verifies.
      const faithful = createCassette({
        mode: "verify",
        fixture: path,
        capture: "content",
      });
      const goodProvider = new MockProviderClient();
      goodProvider.setScript(toolScript);
      let faithfulToolRuns = 0;
      const faithfulRun = await setup(
        () => faithful.provider(goodProvider),
        faithful.wrapTool(
          makeNoteTool(() => {
            faithfulToolRuns += 1;
          }),
        ),
      );
      const faithfulResult = await driveTurn(faithfulRun);
      await faithful.close();
      expect(faithfulToolRuns).toBe(1);
      expect(faithfulResult.events.map((e) => e.type)).toContain(
        "run.completed",
      );

      // Divergent: the model answers differently than the fixture pins; the
      // live re-execution must fail closed on the trace comparison.
      const divergent = createCassette({
        mode: "verify",
        fixture: path,
        capture: "content",
      });
      const badProvider = new MockProviderClient();
      badProvider.setScript([
        toolScript[0] as MockTurn,
        { steps: [{ kind: "text", content: "COMPLETELY DIFFERENT" }] },
      ]);
      const divergentRun = await setup(
        () => divergent.provider(badProvider),
        divergent.wrapTool(makeNoteTool(() => undefined)),
      );
      const divergentResult = await driveTurn(divergentRun);
      expect(divergentResult.events.map((e) => e.type)).toContain("run.failed");
      // The mismatch surfaced DURING the run (the request itself matched, the
      // live stream did not); every effect was still consumed, so close() is
      // clean — the durable run.failed is the failure evidence.
      await divergent.close();
    } finally {
      await cleanup();
    }
  }, 30_000);
});
