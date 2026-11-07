/**
 * AGENTKIT-81, client leg: the `agentkit.stream.resync` control frame the
 * server emits when an unknown `Last-Event-ID` earned a full replay is
 * consumed at the connection layer — never yielded as a run event, never
 * mistaken for the settlement marker — and the replay it announces dedupes
 * exactly like a silent one did. A marker the server did not write is
 * protocol drift and raises, rather than being silently swallowed.
 *
 * Scripted `fetch` for the reason `stream-bounds.test.ts` gives: a correct
 * server writes the marker byte-exactly; only a fake can write it wrong.
 */
import { describe, expect, test } from "bun:test";
import { CONTRACT_VERSION, type AiRunEvent } from "@agentkit/contracts";
import { createAgentKitClient } from "../src/index.js";

const BASE_URL = "http://resync.test";

const RESYNC_FRAME =
  'event: agentkit.stream.resync\ndata: {"reason":"unknown_last_event_id"}\n\n';
const SETTLED_FRAME = "event: agentkit.stream.settled\ndata: {}\n\n";

function event(seq: number): AiRunEvent {
  return {
    type: "run.message.delta",
    runId: "run-1",
    timestamp: new Date(seq * 1000).toISOString(),
    contractVersion: CONTRACT_VERSION,
    eventId: `evt-${seq}`,
    seq,
    data: { delta: `tok-${seq}` },
  } as AiRunEvent;
}

function frameText(...events: AiRunEvent[]): string {
  return events
    .map((e) => `id: ${e.eventId}\ndata: ${JSON.stringify(e)}\n\n`)
    .join("");
}

describe("the resync marker", () => {
  test("is consumed, never yielded, and its announced replay dedupes", async () => {
    let opened = 0;
    const client = createAgentKitClient({
      baseUrl: BASE_URL,
      fetch: async () => {
        if (opened++ === 0) {
          // Dies after one event, without a settlement marker: the client
          // reconnects, cursor `evt-0` in hand.
          return new Response(new TextEncoder().encode(frameText(event(0))), {
            status: 200,
          });
        }
        // The server cannot see the cursor (it never recognised the id) and
        // replays EVERYTHING — now announced, not silent.
        return new Response(
          new TextEncoder().encode(
            RESYNC_FRAME +
              frameText(event(0), event(1), event(2)) +
              SETTLED_FRAME,
          ),
          { status: 200 },
        );
      },
    });
    const seen: AiRunEvent[] = [];
    for await (const event of client.streamRun("run-1", { retryDelayMs: 1 })) {
      seen.push(event);
    }
    // e0 once from the first connection; the announced replay of it deduped.
    expect(seen.map((event) => event.seq)).toEqual([0, 1, 2]);
    expect(opened).toBe(2);
  });

  test("a malformed marker is protocol drift, not a silent drop", async () => {
    const client = createAgentKitClient({
      baseUrl: BASE_URL,
      fetch: async () => {
        // Not the byte-exact marker the server writes: `{}` carries no reason.
        return new Response(
          new TextEncoder().encode(
            "event: agentkit.stream.resync\ndata: {}\n\n",
          ),
          { status: 200 },
        );
      },
    });
    const iterate = async (): Promise<void> => {
      for await (const _event of client.streamRun("run-1", {
        maxRetries: 1,
        retryDelayMs: 1,
      })) {
      }
    };
    await expect(iterate()).rejects.toThrow(
      "Run event stream carried an invalid resync marker.",
    );
  });
});
