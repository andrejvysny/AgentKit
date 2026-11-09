/**
 * A real HTTP server for the client to talk to: the Rust `http_fixture`.
 *
 * The client's whole job is HTTP and SSE framing, so testing it against a
 * hand-written stub would test the stub: the resume path in particular only
 * means anything against a server that really replays from `Last-Event-ID`, over
 * a body that really arrives in chunks. The fixture binary (built on demand by
 * `rust/tests/support/http-server.ts`) is the only backend; scripted providers,
 * the task-store peeks and the secret reads cross its private test-control
 * listener — see `./rust-server.ts`.
 */
import type { AiRunEvent } from "@agentkit/contracts";
import type { TaskStore } from "@agentkit/host";
import {
  createTestEventStamper,
  MockProviderClient,
  nowIso,
} from "@agentkit/testing";
import {
  startRustClientServer,
  type NativeClientServer,
} from "./rust-server.js";

export const TEST_CHAT_ID = "chat-client";
export const SEEDED_API_KEY = "sk-never-published-abcdef";

/** The slice of the task store the native test-control listener exposes. */
export interface TestTaskStore {
  tasks: Pick<
    TaskStore,
    | "getTask"
    | "listByScope"
    | "listEvents"
    | "acquireLease"
    | "appendEvents"
    | "nextSeq"
  >;
}

/** Plaintext secrets as the native test-control listener reveals them. */
export interface TestSecretStore {
  readonly values: Map<string, string>;
  get(ref: string): Promise<string | null>;
}

export interface TestServer {
  baseUrl: string;
  store: TestTaskStore;
  provider: MockProviderClient;
  secrets: TestSecretStore;
  stop(): Promise<void>;
}

export interface TestServerOptions {
  /** The turn the mock provider plays. Defaults to a single short answer. */
  provider?: MockProviderClient;
  /** Gateway-side `authenticate` for the auth-header test. */
  deps?: {
    authenticate?: (request: Request) => Promise<unknown> | unknown;
  };
}

export async function startTestServer(
  options: TestServerOptions = {},
): Promise<TestServer> {
  const server: NativeClientServer = await startRustClientServer(options);
  return server;
}

/** A provider that streams `count` deltas — a run long enough to cut in half. */
export function chattyProvider(count: number): MockProviderClient {
  const provider = new MockProviderClient();
  provider.setScript([
    {
      steps: Array.from({ length: count }, (_unused, index) => ({
        kind: "text" as const,
        content: `chunk-${String(index).padStart(3, "0")} `,
      })),
    },
  ]);
  return provider;
}

/**
 * A provider that streams one delta and then PARKS until the run is cancelled.
 *
 * A `MockProviderClient` subclass rather than `@agentkit/testing`'s
 * `HangingProviderClient` only because the fixture here is typed to the mock —
 * the behaviour is the same, and it is what makes "the run was demonstrably
 * still live" a fact of the test rather than a race against a fast provider.
 */
export class ParkingProvider extends MockProviderClient {
  private announce!: () => void;
  private announceStopped!: () => void;
  /** Resolves the first time the stream parks. */
  readonly parked: Promise<void> = new Promise<void>((resolve) => {
    this.announce = resolve;
  });
  readonly stopped: Promise<void> = new Promise<void>((resolve) => {
    this.announceStopped = resolve;
  });
  cancelled = false;

  override async *streamChat(
    input: Parameters<MockProviderClient["streamChat"]>[0],
  ): AsyncIterable<AiRunEvent> {
    const stamp = createTestEventStamper();
    yield stamp({
      type: "run.started",
      runId: input.runId,
      timestamp: nowIso(),
      data: { model: input.model, toolCount: 0 },
    });
    yield stamp({
      type: "run.message.delta",
      runId: input.runId,
      timestamp: nowIso(),
      data: { delta: "one" },
    });
    try {
      this.announce();
      await new Promise<void>((resolve) => {
        const signal = input.signal;
        if (signal === undefined || signal.aborted) {
          resolve();
          return;
        }
        signal.addEventListener("abort", () => resolve(), { once: true });
      });
      this.cancelled = input.signal?.aborted === true;
      const aborted = new Error("The operation was aborted.");
      aborted.name = "AbortError";
      throw aborted;
    } finally {
      this.announceStopped();
    }
  }
}

/** Poll until `predicate` holds, or fail loudly about what never happened. */
export async function waitFor(
  predicate: () => Promise<boolean>,
  what: string,
  // Real-socket e2e cases have flaked under concurrent test load with the old
  // 5 s budget; a generous ceiling costs nothing when the predicate holds early.
  timeoutMs = 10_000,
): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (await predicate()) return;
    await new Promise((resolve) => setTimeout(resolve, 2));
  }
  throw new Error(`Timed out waiting for ${what}.`);
}
