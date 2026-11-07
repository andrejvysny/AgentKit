import { testServiceOptions } from "./helpers/test-profiles.js";
/**
 * W1b — an invocation that the runtime already gave up on must NEVER enter
 * user code.
 *
 * The witness this pins: a tool envelope is queued to the JS thread, the
 * executor abandons the bridge future at the tool timeout while the thread
 * is blocked, and the late envelope used to run the user callback anyway —
 * an abandoned invocation acting after the run had durably settled. The
 * router now claims each call SYNCHRONOUSLY from the Rust bridge
 * (`claimCall`) before `dispatchUser`/`withAbort`; a failed claim settles
 * nothing (Rust already did) and enters nothing. The per-call
 * AbortController stays a mid-execution signal, not the liveness gate.
 *
 * The provider mock runs in a SEPARATE Bun process
 * (`fixtures/liveness-provider-fixture.ts`): an in-process mock would stall
 * with the blocked main thread and the turn could never settle during the
 * block, hiding the defect instead of exposing it.
 */
import { expect, test } from "bun:test";
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { NativeSessionService, NativeStore } from "../src/index.js";
import { available } from "./helpers/binding.js";

const nativeTest = available ? test : test.skip;

const FIXTURE = join(
  import.meta.dir,
  "fixtures",
  "liveness-provider-fixture.ts",
);

interface FixtureProvider {
  baseUrl: string;
  /** Completion requests answered, counted from the fixture's stdout. */
  requests(): number;
  kill(): void;
}

async function startFixtureProvider(): Promise<FixtureProvider> {
  const proc = Bun.spawn(["bun", FIXTURE, "0"], {
    stdout: "pipe",
    stderr: "pipe",
  });
  let requests = 0;
  const reader = proc.stdout.getReader();
  const decoder = new TextDecoder();
  let buffered = "";
  let port = 0;
  while (port === 0) {
    const { value, done } = await reader.read();
    if (done) throw new Error("fixture provider died before LISTENING");
    buffered += decoder.decode(value, { stream: true });
    const listening = /LISTENING (\d+)/.exec(buffered);
    if (listening) port = Number(listening[1]);
  }
  // Keep draining in the background so REQUEST lines keep counting while
  // the test runs (and while its main thread is blocked).
  void (async () => {
    for (;;) {
      const { value, done } = await reader.read();
      if (done) return;
      requests += (
        decoder.decode(value, { stream: true }).match(/REQUEST \d+/g) ?? []
      ).length;
    }
  })();
  return {
    baseUrl: `http://127.0.0.1:${port}/v1`,
    requests: () => requests,
    kill: () => proc.kill(),
  };
}

async function openService(provider: FixtureProvider): Promise<{
  store: NativeStore;
  service: NativeSessionService;
}> {
  const store = await NativeStore.open(
    join(mkdtempSync(join(tmpdir(), "agentkit-liveness-")), "store.sqlite"),
  );
  const connectionId = await NativeSessionService.registerProviderConnection(
    provider.baseUrl,
    null,
  );
  const service = await store.createService(
    testServiceOptions({
      jsToolAuthority: { configuredMode: "full_auto" },
      providerConfig: {
        protocol: "chat-completions",
        providerId: "probe",
        connectionId,
        model: "loopback-model",
      },
    }),
  );
  return { store, service };
}

async function turnStatus(
  service: NativeSessionService,
  sessionId: string,
  turnId: string,
): Promise<string | undefined> {
  const turns = (await service.listTurns(sessionId)) as {
    id?: string;
    turnId?: string;
    status?: string;
  }[];
  return turns.find((t) => t.id === turnId || t.turnId === turnId)?.status;
}

async function waitFor(
  condition: () => Promise<boolean> | boolean,
  timeoutMs: number,
): Promise<boolean> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (await condition()) return true;
    await Bun.sleep(25);
  }
  return false;
}

interface TurnOutcome {
  entries: number;
  terminal: boolean;
  providerRequests: number;
}

/** Drive one `probe_liveness` invocation with a 100ms tool timeout,
 * optionally busy-blocking the JS thread for `blockMs` right after the
 * turn starts. */
async function driveBlockedTurn(blockMs: number): Promise<TurnOutcome> {
  const provider = await startFixtureProvider();
  const { store, service } = await openService(provider);
  let entries = 0;
  try {
    await service.registerTool(
      {
        name: "probe_liveness",
        description: "counts entries into user code",
        timeoutMs: 100,
        inputSchema: { type: "object", properties: {} },
      },
      async () => {
        entries += 1;
        return { data: "ran" };
      },
    );
    const session = await service.createSession({ profileId: "coding" });
    const sessionId = String(session.id ?? session.sessionId);
    const sentPromise = service.send(sessionId, { content: "go" });
    if (blockMs > 0) {
      const t0 = Date.now();
      while (Date.now() - t0 < blockMs) {
        /* busy-block the JS event loop across the tool timeout window */
      }
    }
    const sent = await sentPromise;
    const turnId = String(
      sent && typeof sent === "object" && "turnId" in sent ? sent.turnId : "",
    );
    const terminal = await waitFor(async () => {
      const status = await turnStatus(service, sessionId, turnId);
      return (
        status === "completed" || status === "failed" || status === "cancelled"
      );
    }, 10_000);
    // Drain the JS event loop twice so any stale envelope queued behind
    // the block is delivered and processed before counting: on a fix that
    // merely delayed dispatch, the counter would still tick here. Real
    // time is deliberate — the tool timeout is the native tokio clock,
    // unreachable by JS fake timers.
    const drain = () => {
      const { promise, resolve } = Promise.withResolvers<void>();
      setTimeout(resolve, 0);
      return promise;
    };
    await drain();
    await drain();
    return { entries, terminal, providerRequests: provider.requests() };
  } finally {
    await service.close().catch(() => undefined);
    await store.close();
    provider.kill();
  }
}

nativeTest(
  "a tool call abandoned at its timeout while the JS thread is blocked never enters user code after resume",
  async () => {
    // Provider answers from a separate process, so the turn settles while
    // this thread is blocked; the 100ms timeout fires and the executor
    // abandons the bridge future mid-block.
    const outcome = await driveBlockedTurn(1_200);
    // The durable record exists and settled without the callback's answer.
    expect(outcome.terminal).toBe(true);
    expect(outcome.providerRequests).toBeGreaterThanOrEqual(2);
    // (1) ZERO entries into the abandoned invocation.
    expect(outcome.entries).toBe(0);
  },
  { timeout: 30_000 },
);

nativeTest(
  "control: without blocking, the same 100ms-timeout callback is claimed and executes exactly once",
  async () => {
    const outcome = await driveBlockedTurn(0);
    expect(outcome.terminal).toBe(true);
    expect(outcome.entries).toBe(1);
  },
  { timeout: 30_000 },
);
