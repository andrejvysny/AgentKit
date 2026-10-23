import { expect, test } from "bun:test";
import { createMcpServerHandler, type McpSessionScope } from "../src/index.js";
import {
  serveHandler,
  connectClient,
  authHeaders,
  TEST_TOKEN,
} from "./helpers.js";

test("request cancellation reaches the running tool and prevents queued calls from executing", async () => {
  let running!: () => void;
  const started = new Promise<void>((resolve) => {
    running = resolve;
  });
  let cancelled!: () => void;
  const observedCancellation = new Promise<void>((resolve) => {
    cancelled = resolve;
  });
  const executions: string[] = [];
  let signal: AbortSignal | undefined;
  const handler = createMcpServerHandler({
    auth: { bearerToken: TEST_TOKEN },
    maxConcurrentCallsPerSession: 1,
    tools: {
      catalog: {
        listTools: async () => [
          {
            namespace: "test",
            definition: {
              name: "test",
              version: "1",
              description: "test",
              effect: "read",
              capability: "test",
              inputSchema: { type: "object" },
            },
          },
        ],
      },
      execute: async (
        _name: string,
        args: unknown,
        scope?: McpSessionScope,
      ) => {
        executions.push((args as { name: string }).name);
        signal = scope?.signal;
        running();
        await new Promise<void>((resolve) => {
          scope?.signal?.addEventListener(
            "abort",
            () => {
              cancelled();
              resolve();
            },
            { once: true },
          );
        });
        return {
          ok: true,
          status: "ok",
          warnings: [],
          truncated: false,
          data: {},
        };
      },
    },
  });
  const served = serveHandler(handler);
  const { client } = await connectClient(served.url, authHeaders());
  try {
    const firstController = new AbortController();
    const queuedController = new AbortController();
    const first = client
      .callTool({ name: "test", arguments: { name: "first" } }, undefined, {
        signal: firstController.signal,
      })
      .catch(() => undefined);
    await started;
    const queued = client
      .callTool({ name: "test", arguments: { name: "queued" } }, undefined, {
        signal: queuedController.signal,
      })
      .catch(() => undefined);
    await new Promise((resolve) => setTimeout(resolve, 5));
    queuedController.abort();
    await queued;
    firstController.abort();
    await first;
    await observedCancellation;
    expect(signal?.aborted).toBe(true);
    expect(executions).toEqual(["first"]);
  } finally {
    await client.close();
    await handler.dispose();
    await served.stop();
  }
});
