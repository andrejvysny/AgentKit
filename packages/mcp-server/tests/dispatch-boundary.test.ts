import { expect, test } from "bun:test";
import { createMcpServerHandler, type McpSessionScope } from "../src/index.js";
import {
  authHeaders,
  connectClient,
  serveHandler,
  TEST_TOKEN,
} from "./helpers.js";

for (const mode of ["legacy", "auto"] as const) {
  test(`${mode} cancellation during catalog wait prevents execution dispatch`, async () => {
    let entered!: () => void;
    let cancelled!: () => void;
    let release!: () => void;
    const catalogEntered = new Promise<void>((resolve) => {
      entered = resolve;
    });
    const cancellationObserved = new Promise<void>((resolve) => {
      cancelled = resolve;
    });
    const catalogGate = new Promise<void>((resolve) => {
      release = resolve;
    });
    let executions = 0;
    const handler = createMcpServerHandler({
      auth: { bearerToken: TEST_TOKEN },
      tools: {
        catalog: {
          async listTools(scope?: McpSessionScope) {
            scope?.signal?.addEventListener("abort", cancelled, { once: true });
            entered();
            await catalogGate;
            return [
              {
                namespace: "test",
                definition: {
                  name: "write",
                  version: "1",
                  effect: "write",
                  capability: "write",
                  description: "write",
                  inputSchema: { type: "object" },
                },
              },
            ];
          },
        },
        async execute() {
          executions++;
          return {
            ok: true,
            status: "ok",
            warnings: [],
            truncated: false,
            data: {},
          };
        },
      },
      writesEnabled: true,
    });
    const served = serveHandler(handler);
    const { client } = await connectClient(served.url, authHeaders(), mode);
    const controller = new AbortController();
    try {
      const call = client
        .callTool(
          { name: "write", arguments: {} },
          { signal: controller.signal },
        )
        .catch(() => undefined);
      await catalogEntered;
      controller.abort();
      await call;
      await cancellationObserved;
      release();
      await Bun.sleep(10);
      expect(executions).toBe(0);
    } finally {
      release();
      await client.close();
      await handler.dispose();
      await served.stop();
    }
  });
}
