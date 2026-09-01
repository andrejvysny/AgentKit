import type { AiTool } from "@agentkit/core";
import type { Clock, ToolSetContributor } from "@agentkit/host";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StreamableHTTPClientTransport } from "@modelcontextprotocol/sdk/client/streamableHttp.js";
import type { McpServerHandler } from "../src/index.js";

export const TEST_TOKEN = "test-token-0123456789";

/** The header `sessionScope` reads in these tests. */
export const CHAT_HEADER = "x-agentkit-chat";

/**
 * A read tool that echoes its input, with a `summary` and a `modelData` that
 * differs from `data` — the two things the envelope projection has to carry.
 */
export function echoTool(): AiTool {
  return {
    definition: {
      name: "demo_echo",
      version: "1.0.0",
      effect: "read",
      capability: "demo.echo",
      description: "Echo the text back.",
      inputSchema: {
        type: "object",
        properties: { text: { type: "string" } },
        required: ["text"],
      },
    },
    async execute(ctx, input) {
      const text = (input as { text: string }).text;
      return {
        ok: true,
        summary: `echoed ${text.length} char(s)`,
        // `data` is the UI-facing payload; `modelData` is what the envelope
        // carries. The test asserts the MCP result shows the latter.
        data: { text, verbose: "ui-only" },
        modelData: { echo: text },
        sources: [],
        warnings: [],
        truncated: false,
        limits: ctx.limits,
      };
    },
  };
}

/** A tool that reports failure without throwing — the `isError` path. */
export function failingTool(): AiTool {
  return {
    definition: {
      name: "demo_fail",
      version: "1.0.0",
      effect: "read",
      capability: "demo.fail",
      description: "Always fails.",
      inputSchema: { type: "object", properties: {} },
    },
    async execute(ctx) {
      return {
        ok: false,
        summary: "it broke",
        data: { errorCode: "demo_broken", errorMessage: "it broke" },
        sources: [],
        warnings: [],
        truncated: false,
        limits: ctx.limits,
      };
    },
  };
}

/** `effect: "write"` — the marker `writesEnabled` filters on. */
export function writeTool(): AiTool {
  return {
    definition: {
      name: "demo_write",
      version: "1.0.0",
      effect: "write",
      capability: "demo.write",
      description: "Stage a write.",
      inputSchema: { type: "object", properties: {} },
    },
    async execute(ctx) {
      return {
        ok: true,
        summary: "staged",
        data: { staged: true },
        sources: [],
        warnings: [],
        truncated: false,
        limits: ctx.limits,
      };
    },
  };
}

/**
 * A contributor whose tool set depends on the chat it is asked about — the only
 * way to prove a session's scope actually reaches the catalogue.
 */
export function demoContributor(): ToolSetContributor {
  return {
    namespace: "demo",
    async contribute(ctx) {
      const base: AiTool[] = [echoTool(), failingTool(), writeTool()];
      if (ctx.chatId === undefined) return base;
      return [...base, scopedTool(ctx.chatId)];
    },
  };
}

/** `demo_only_<chatId>` — visible only to the session scoped to that chat. */
export function scopedTool(chatId: string): AiTool {
  const name = `demo_only_${chatId.replace(/[^a-zA-Z0-9_-]/g, "_")}`;
  return {
    definition: {
      name,
      version: "1.0.0",
      effect: "read",
      capability: "demo.scoped",
      description: `Only for ${chatId}.`,
      inputSchema: { type: "object", properties: {} },
    },
    async execute(ctx) {
      return {
        ok: true,
        summary: chatId,
        data: { chatId },
        sources: [],
        warnings: [],
        truncated: false,
        limits: ctx.limits,
      };
    },
  };
}

export interface ServedHandler {
  url: URL;
  port: number;
  stop(): Promise<void>;
}

/**
 * Put a handler on a real loopback socket at an ephemeral port.
 *
 * The official SDK client only speaks to a URL, so an in-process test of the
 * fetch handler still has to be reachable over HTTP. `127.0.0.1:<port>` is
 * inside the handler's DEFAULT `allowedHosts`, which is the point: the happy
 * path here is the shipped default configuration, not a relaxed one.
 */
export function serveHandler(handler: McpServerHandler): ServedHandler {
  const server = Bun.serve({
    hostname: "127.0.0.1",
    port: 0,
    fetch: (request) => handler.fetch(request),
  });
  const port = Number(server.port);
  return {
    url: new URL(`http://127.0.0.1:${port}/mcp`),
    port,
    async stop() {
      await server.stop(true);
    },
  };
}

/** An SDK client connected over streamable HTTP with the given extra headers. */
export async function connectClient(
  url: URL,
  headers: Record<string, string>,
): Promise<{
  client: Client;
  transport: StreamableHTTPClientTransport;
}> {
  const transport = new StreamableHTTPClientTransport(url, {
    requestInit: { headers },
  });
  const client = new Client({ name: "test-client", version: "1.0.0" });
  await client.connect(transport);
  return { client, transport };
}

/** `Authorization` plus whatever else a test wants. */
export function authHeaders(
  extra: Record<string, string> = {},
): Record<string, string> {
  return { authorization: `Bearer ${TEST_TOKEN}`, ...extra };
}

/**
 * Read one JSON-RPC response out of a raw handler response, whether the
 * transport answered with `application/json` or an SSE frame.
 */
export async function readRpcResponse(
  response: Response,
): Promise<Record<string, unknown>> {
  const text = await response.text();
  const contentType = response.headers.get("content-type") ?? "";
  if (contentType.includes("application/json")) {
    return JSON.parse(text) as Record<string, unknown>;
  }
  for (const line of text.split("\n")) {
    if (line.startsWith("data:")) {
      return JSON.parse(line.slice(5).trim()) as Record<string, unknown>;
    }
  }
  throw new Error(`no JSON-RPC frame in response body: ${text}`);
}

/**
 * The `text` of one content block of a `tools/call` result.
 *
 * MCP content blocks are a union (text / image / audio / resource), so reading
 * `.text` off one needs a narrow. Throwing on a non-text block is the point:
 * this server only ever emits text, and a test that silently read `undefined`
 * would pass while the projection changed shape underneath it.
 */
export function textBlock(
  content: readonly { type: string; text?: unknown }[],
  index: number,
): string {
  const block = content[index];
  if (block?.type !== "text" || typeof block.text !== "string") {
    throw new Error(
      `content[${index}] is not a text block: ${JSON.stringify(block)}`,
    );
  }
  return block.text;
}

/** A clock that starts fixed and only moves when a test says so. */
export interface TestClock extends Clock {
  advance(ms: number): void;
}

export function createTestClock(start = "2026-01-01T00:00:00.000Z"): TestClock {
  let current = new Date(start).getTime();
  return {
    now: () => new Date(current),
    nowIso: () => new Date(current).toISOString(),
    advance: (ms: number) => {
      current += ms;
    },
  };
}
