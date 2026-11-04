/**
 * Out-of-process Responses-protocol provider for the provider-continuity
 * tests. It must outlive the application process that registers the
 * provider connection — the whole point of the scenario is that the
 * provider keeps answering across REAL application restarts, so the mock
 * cannot live in either Bun process under test.
 *
 * It answers `POST` to any path ending in `/responses` with a single
 * `response.completed` SSE frame carrying one encrypted reasoning item and
 * one assistant message: exactly the shape whose encrypted items the
 * durable continuation stores and replays (see
 * `tests/provider-lane.test.ts` responses suite).
 *
 * argv: `[port, expectedKey?]`. When `expectedKey` is a non-empty string
 * the fixture answers 401 for any other `authorization` header — the
 * wrong-credentials witness.
 *
 * Observability for the parent test (stdout lines):
 *   LISTENING <port>            once at startup
 *   REQUEST <n> enc=<bool>      per answered request; `enc` records whether
 *                               the request body replayed stored encrypted
 *                               reasoning (the continuation witness)
 *   DENIED <n> auth=<header>    per 401 rejection
 */
const port = Number(process.argv[2] ?? 0);
const expectedKey = process.argv[3] ?? "";

let requests = 0;

const server = Bun.serve({
  hostname: "127.0.0.1",
  port,
  async fetch(request) {
    const url = new URL(request.url);
    if (request.method !== "POST" || !url.pathname.endsWith("/responses"))
      return new Response("not found", { status: 404 });
    const auth = request.headers.get("authorization") ?? "";
    const raw = await request.text();
    requests += 1;
    if (expectedKey !== "" && auth !== `Bearer ${expectedKey}`) {
      console.log(`DENIED ${requests} auth=${auth}`);
      return new Response(
        JSON.stringify({
          error: { code: "invalid_api_key", message: "bad key" },
        }),
        {
          status: 401,
          headers: { "content-type": "application/json" },
        },
      );
    }
    console.log(`REQUEST ${requests} enc=${raw.includes("enc_continuity")}`);
    const frame = {
      type: "response.completed",
      response: {
        id: `resp_fixture_${requests}`,
        status: "completed",
        output: [
          {
            id: `reasoning-${requests}`,
            type: "reasoning",
            encrypted_content: `enc_continuity_${requests}`,
          },
          {
            id: `message-${requests}`,
            type: "message",
            role: "assistant",
            status: "completed",
            content: [{ type: "output_text", text: `turn ${requests} done` }],
          },
        ],
      },
    };
    return new Response(`data: ${JSON.stringify(frame)}\n\ndata: [DONE]\n\n`, {
      headers: { "content-type": "text/event-stream" },
    });
  },
});

console.log(`LISTENING ${server.port}`);

export {};
