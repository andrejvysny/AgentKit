import { expect, test } from "bun:test";
import {
  nativeBackendAvailable,
  startNativeServer,
} from "../../../rust/tests/support/http-server.js";
import { createAgentKitClient } from "../src/index.js";

test.skipIf(!nativeBackendAvailable)(
  "TypeScript client resumes Rust SSE at every byte boundary",
  async () => {
  const server = await startNativeServer();
  try {
    const chat = await server.client.createChat({});
    const submitted = await server.client.submitMessage(
      { chatId: chat.id },
      { content: "Unicode Ž🙂" },
    );
    const runId = submitted.result.runId;
    const expected: string[] = [];
    for await (const event of server.client.streamRun(runId))
      expected.push(event.eventId);
    const response = await fetch(`${server.baseUrl}/v1/runs/${runId}/stream`, {
      headers: { authorization: `Bearer ${server.token}` },
    });
    const wire = new Uint8Array(await response.arrayBuffer());
    expect(wire.byteLength).toBeGreaterThan(0);
    let nextCut = 0;
    const checkCuts = async () => {
      while (nextCut < wire.byteLength) {
        const cut = nextCut++;
        let first = true;
        const client = createAgentKitClient({
          baseUrl: server.baseUrl,
          headers: () => ({ authorization: `Bearer ${server.token}` }),
          fetch: async (url, input) => {
            if (first && url.includes("/stream")) {
              first = false;
              return new Response(wire.slice(0, cut), {
                headers: { "content-type": "text/event-stream" },
              });
            }
            return fetch(url, input);
          },
        });
        const actual: string[] = [];
        for await (const event of client.streamRun(runId, {
          retryDelayMs: 0,
          maxRetries: 1,
          maxTotalReconnects: 1,
        }))
          actual.push(event.eventId);
        expect(actual, `cut at byte ${cut}`).toEqual(expected);
      }
    };
    await Promise.all(Array.from({ length: 32 }, checkCuts));
  } finally {
    await server.stop();
  }
}, 120_000);
