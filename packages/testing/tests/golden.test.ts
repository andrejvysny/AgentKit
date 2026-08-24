import { describe, expect, it } from "bun:test";
import { runChat, AiToolRegistry, resolveToolLimits } from "@agentkit/core";
import { CONTRACT_VERSION, type AiRunEvent } from "@agentkit/contracts";
import {
  GOLDEN_TRACE_NAMES,
  loadGoldenTrace,
  assertMatchesGolden,
} from "../src/golden/golden.js";
import { MockProviderClient } from "../src/mock-provider.js";
import { makeUserMessage } from "../src/fixtures.js";

const TERMINAL = new Set(["run.completed", "run.failed", "run.cancelled"]);

describe("golden traces", () => {
  for (const name of GOLDEN_TRACE_NAMES) {
    describe(name, () => {
      it("loads, is non-empty, and has a strictly increasing seq from 0", () => {
        const events = loadGoldenTrace(name);
        expect(events.length).toBeGreaterThan(0);
        expect(events.map((e) => e.seq)).toEqual(events.map((_, i) => i));
      });

      it("has exactly one terminal event, which is the last one", () => {
        const events = loadGoldenTrace(name);
        const terminals = events.filter((e) => TERMINAL.has(e.type));
        expect(terminals.length).toBe(1);
        expect(events.at(-1)).toBe(terminals[0]);
      });

      it("stamps the CURRENT contractVersion on every event", () => {
        // Pinned to the live constant, not merely "some non-empty string": a
        // contract bump that nobody re-recorded against must fail all five
        // traces here, rather than waiting for the chat-only replay below to
        // be the single test that notices.
        const events = loadGoldenTrace(name);
        for (const e of events) {
          expect(e.contractVersion).toBe(CONTRACT_VERSION);
        }
      });
    });
  }

  // A replay self-test: re-run the chat-only scenario live against the real
  // run-loop and confirm it still produces the same (normalized) events as
  // the frozen trace. This is what would catch a run-loop change that
  // silently altered the chat-only event shape without anyone regenerating
  // the golden trace.
  it("replays chat-only live and matches the golden trace", async () => {
    const client = new MockProviderClient();
    client.setScript([
      {
        steps: [
          { kind: "text", content: "Hello from the golden chat-only trace." },
        ],
      },
    ]);
    const events: AiRunEvent[] = [];
    const gen = runChat({
      client,
      registry: new AiToolRegistry(),
      model: "gpt-golden",
      messages: [makeUserMessage("hi")],
      limits: resolveToolLimits({ preference: "small" }),
      runId: "run-golden-chat-only",
      firstSeq: 0,
    });
    for (;;) {
      const next = await gen.next();
      if (next.done) break;
      events.push(next.value);
    }
    expect(() => assertMatchesGolden(events, "chat-only")).not.toThrow();
  });
});
