/**
 * Out-of-process scripted provider for the invocation-liveness tests.
 *
 * The blocked-event-loop witness must keep answering provider HTTP while
 * the test's main JS thread is blocked: an in-process `Bun.serve` would
 * stall with it and the run could never settle during the block. So this
 * fixture runs as its own Bun process (`bun <this file> <port>`) and
 * answers `POST` to any path ending in `/chat/completions`, in request
 * order:
 *
 *   request 1 → one complete tool call for `probe_liveness`
 *   request 2+ → final content "done"
 *
 * It prints `LISTENING <port>` on startup so the test can wait for it, and
 * one `REQUEST <n>` line per answered completion request.
 */
const port = Number(process.argv[2] ?? 0);

let requests = 0;

const server = Bun.serve({
  hostname: "127.0.0.1",
  port,
  async fetch(request) {
    const url = new URL(request.url);
    if (
      request.method !== "POST" ||
      !url.pathname.endsWith("/chat/completions")
    )
      return new Response("not found", { status: 404 });
    await request.json();
    requests += 1;
    // Observability for the parent test: proves the turn settled while the
    // parent's JS thread was blocked (an in-process mock could not).
    console.log(`REQUEST ${requests}`);
    const chunk = (
      id: number,
      delta: Record<string, unknown>,
      finish: string | null,
    ) =>
      `data: ${JSON.stringify({
        id: `chatcmpl-${id}`,
        object: "chat.completion.chunk",
        created: 1,
        model: "loopback-model",
        choices: [{ index: 0, delta, finish_reason: finish }],
      })}\n\n`;
    let body: string;
    if (requests === 1) {
      body =
        chunk(1, { role: "assistant" }, null) +
        chunk(
          2,
          {
            tool_calls: [
              {
                index: 0,
                id: "call-liveness-1",
                type: "function",
                function: { name: "probe_liveness", arguments: "{}" },
              },
            ],
          },
          "tool_calls",
        ) +
        "data: [DONE]\n\n";
    } else {
      body =
        chunk(1, { role: "assistant" }, null) +
        chunk(2, { content: "done" }, "stop") +
        "data: [DONE]\n\n";
    }
    return new Response(body, {
      headers: { "content-type": "text/event-stream" },
    });
  },
});

console.log(`LISTENING ${server.port}`);

export {};
