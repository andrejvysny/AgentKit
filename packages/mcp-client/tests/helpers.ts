import type { Clock, SecretStore } from "@agentkit/host";
import { resolveToolLimits } from "@agentkit/core";
import type { AiToolExecutionContext } from "@agentkit/core";
import { Server } from "@modelcontextprotocol/sdk/server/index.js";
import { InMemoryTransport } from "@modelcontextprotocol/sdk/inMemory.js";
import {
  CallToolRequestSchema,
  ListToolsRequestSchema,
} from "@modelcontextprotocol/sdk/types.js";
import type { Transport } from "@modelcontextprotocol/sdk/shared/transport.js";
import type { McpTransportFactory, McpTransportRequest } from "../src/index.js";

/** A clock that starts fixed and only moves when a test says so. */
export interface TestClock extends Clock {
  advance(ms: number): void;
}

export function createTestClock(start = "2026-01-01T00:00:00.000Z"): TestClock {
  let current = new Date(start).getTime();
  return {
    now: () => new Date(current),
    nowIso: () => new Date(current).toISOString(),
    advance: (ms) => {
      current += ms;
    },
  };
}

export function createSecretStore(
  seed: Record<string, string> = {},
): SecretStore & { store: Map<string, string> } {
  const store = new Map(Object.entries(seed));
  return {
    store,
    async get(ref) {
      return store.get(ref) ?? null;
    },
    async set(ref, value) {
      store.set(ref, value);
    },
    async delete(ref) {
      store.delete(ref);
    },
    async listRefs() {
      return [...store.keys()];
    },
  };
}

export interface FakeToolResult {
  content: { type: string; text?: string; [key: string]: unknown }[];
  structuredContent?: Record<string, unknown>;
  isError?: boolean;
}

export interface FakeToolSpec {
  name: string;
  description?: string;
  /** Passed to the client VERBATIM — the point of several assertions. */
  inputSchema?: Record<string, unknown>;
  annotations?: Record<string, unknown>;
  handler?(
    args: Record<string, unknown> | undefined,
  ): FakeToolResult | Promise<FakeToolResult>;
}

/**
 * A real MCP server over the SDK's low-level `Server`.
 *
 * Low-level rather than `McpServer` on purpose: `McpServer` derives
 * `inputSchema` from Zod, and the tests here assert that whatever the server
 * publishes reaches the tool definition byte-for-byte. Writing the JSON Schema
 * by hand is the only way to tell "passed through" from "regenerated".
 */
export function buildFakeServer(
  name: string,
  tools: readonly FakeToolSpec[],
): Server {
  const server = new Server(
    { name, version: "1.0.0" },
    { capabilities: { tools: {} } },
  );
  server.setRequestHandler(ListToolsRequestSchema, async () => ({
    tools: tools.map((tool) => ({
      name: tool.name,
      ...(tool.description === undefined ? {} : { description: tool.description }),
      inputSchema: (tool.inputSchema ?? { type: "object" }) as {
        type: "object";
      },
      ...(tool.annotations === undefined ? {} : { annotations: tool.annotations }),
    })),
  }));
  server.setRequestHandler(CallToolRequestSchema, async (request) => {
    const spec = tools.find((tool) => tool.name === request.params.name);
    if (!spec) {
      return {
        content: [{ type: "text" as const, text: `unknown tool` }],
        isError: true,
      };
    }
    const result = await (spec.handler?.(request.params.arguments) ?? {
      content: [
        {
          type: "text" as const,
          text: JSON.stringify(request.params.arguments ?? {}),
        },
      ],
    });
    return result as { content: { type: "text"; text: string }[] };
  });
  return server;
}

/** Builds the server that backs the Nth connect attempt for an alias. */
export type ServerBuilder = (attempt: number) => Server | Promise<Server>;

/**
 * A transport factory over {@link InMemoryTransport}, plus the bookkeeping the
 * resilience tests assert on (how many transports were built, and which server
 * is currently live).
 *
 * A builder that throws stands in for a server that cannot be reached — the only
 * honest way to exercise the connect cycle without a real process or socket.
 */
export class InMemoryHarness {
  private readonly attempts = new Map<string, number>();
  private readonly live = new Map<string, Server>();
  private readonly all: Server[] = [];

  constructor(private readonly builders: Record<string, ServerBuilder>) {}

  readonly factory: McpTransportFactory = async (
    request: McpTransportRequest,
  ): Promise<Transport> => {
    const attempt = (this.attempts.get(request.alias) ?? 0) + 1;
    this.attempts.set(request.alias, attempt);
    const builder = this.builders[request.alias];
    if (!builder) throw new Error(`no builder for alias ${request.alias}`);
    const server = await builder(attempt);
    const [clientTransport, serverTransport] =
      InMemoryTransport.createLinkedPair();
    await server.connect(serverTransport);
    this.live.set(request.alias, server);
    this.all.push(server);
    return clientTransport;
  };

  /** How many transports were built for `alias` — i.e. how many connect attempts ran. */
  connects(alias: string): number {
    return this.attempts.get(alias) ?? 0;
  }

  current(alias: string): Server | undefined {
    return this.live.get(alias);
  }

  /** Kill the live server for `alias`, which closes the client side too. */
  async dropCurrent(alias: string): Promise<void> {
    const server = this.live.get(alias);
    this.live.delete(alias);
    if (server) await server.close();
  }

  async closeAll(): Promise<void> {
    for (const server of this.all) {
      try {
        await server.close();
      } catch {
        // Already closed by a drop or a dispose.
      }
    }
  }
}

export function toolContext(
  overrides: Partial<AiToolExecutionContext> = {},
): AiToolExecutionContext {
  return {
    runId: "run-1",
    chatId: "chat-1",
    bindings: [],
    limits: resolveToolLimits({ preference: "small" }),
    ...overrides,
  };
}

/** A promise a test resolves by hand, for "this call is still in flight" scenarios. */
export function deferred<T>(): {
  promise: Promise<T>;
  resolve(value: T): void;
} {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((res) => {
    resolve = res;
  });
  return { promise, resolve };
}
