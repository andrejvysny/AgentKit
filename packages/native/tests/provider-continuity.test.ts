/**
 * P1-E — the Responses continuation must survive a REAL application
 * restart.
 *
 * The reviewer's repro: two separate Bun processes over one on-disk store
 * and one provider config. Process 1 completes a turn storing encrypted
 * continuation state, exits. Process 2 registers the same provider config
 * and submits the next turn — and used to fail BEFORE transport with
 * `provider_continuation_scope_mismatch`, because registration handed out
 * a process-local RANDOM connection id every time while the durable
 * continuation scope bound the previous process's identity. The fix gives
 * a keyed registration (`registerProviderConnection(url, key, {
 * connectionKey, storePath })`) a durable, non-secret logical identity —
 * same key + same credentials across processes = same connection id and
 * generation; a different credential rotates the generation under the
 * same identity; the scope check itself stays strict (wrong providerId or
 * model still rejects before transport).
 *
 * The provider mock and every application process are SEPARATE Bun
 * processes (`fixtures/continuation-provider-fixture.ts`,
 * `fixtures/continuation-turn-fixture.ts`): the registry is
 * binding-process-global, so an in-process second "restart" would not
 * reproduce the defect — and the mock must answer while no app process is
 * alive to serve it.
 */
import { expect, test } from "bun:test";
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { NativeAssistantStore, NativeStore } from "../src/index.js";
import { available } from "./helpers/binding.js";

const nativeTest = available ? test : test.skip;

const PROVIDER_FIXTURE = join(
  import.meta.dir,
  "fixtures",
  "continuation-provider-fixture.ts",
);
const TURN_FIXTURE = join(
  import.meta.dir,
  "fixtures",
  "continuation-turn-fixture.ts",
);

interface FixtureProvider {
  baseUrl: string;
  /** Answered completion requests, counted from the fixture's stdout. */
  requests(): number;
  /** Lines the fixture printed: REQUEST/DENIED witnesses. */
  lines(): string[];
  kill(): void;
}

async function startFixtureProvider(
  expectedKey?: string,
): Promise<FixtureProvider> {
  const proc = Bun.spawn(["bun", PROVIDER_FIXTURE, "0", expectedKey ?? ""], {
    stdout: "pipe",
    stderr: "pipe",
  });
  const reader = proc.stdout.getReader();
  const decoder = new TextDecoder();
  let _buffered = "";
  let port = 0;
  const lines: string[] = [];
  const consume = (chunk: string): void => {
    const listening = /LISTENING (\d+)/.exec(chunk);
    if (listening && port === 0) port = Number(listening[1]);
    for (const line of chunk.split("\n"))
      if (/^(REQUEST|DENIED) /.test(line)) lines.push(line);
  };
  while (port === 0) {
    const { value, done } = await reader.read();
    if (done) throw new Error("fixture provider died before LISTENING");
    const chunk = decoder.decode(value, { stream: true });
    _buffered += chunk;
    consume(chunk);
  }
  void (async () => {
    for (;;) {
      const { value, done } = await reader.read();
      if (done) return;
      consume(decoder.decode(value, { stream: true }));
    }
  })();
  return {
    baseUrl: `http://127.0.0.1:${port}/v1`,
    requests: () => lines.filter((line) => line.startsWith("REQUEST ")).length,
    lines: () => [...lines],
    kill: () => proc.kill(),
  };
}

interface TurnResult {
  connectionId: string;
  turnId: string;
  status: string;
  error: string | null;
}

interface TurnOptions {
  storePath: string;
  baseUrl: string;
  apiKey: string | null;
  phase: number;
  chatId: string;
  connectionKey?: string;
  providerId?: string;
  revokeAndReRegister?: boolean;
  restoreContinuity?: {
    expectedGeneration: number;
    previousGeneration: number;
    confirmUnchangedScope: true;
  };
  foreignConnection?: boolean;
}

/** Run one application process (fresh binding registry) through one turn. */
async function runTurn(options: TurnOptions): Promise<TurnResult> {
  const proc = Bun.spawn(["bun", TURN_FIXTURE, JSON.stringify(options)], {
    stdout: "pipe",
    stderr: "pipe",
  });
  const output = await new Response(proc.stdout).text();
  const failure = await new Response(proc.stderr).text();
  const code = await proc.exited;
  const result = /RESULT (\{.*\})/.exec(output);
  if (!result)
    throw new Error(
      `turn fixture produced no RESULT (exit ${code}): ${output} ${failure}`,
    );
  return JSON.parse(result[1]!) as TurnResult;
}

function storeDir(): string {
  return mkdtempSync(join(tmpdir(), "agentkit-continuity-"));
}

nativeTest(
  "a Responses continuation completes its next turn in a second, fully separate application process",
  async () => {
    const provider = await startFixtureProvider("sk-continuity");
    const dir = storeDir();
    const storePath = join(dir, "store.sqlite");
    const chatId = "continuity-chat";
    const connectionKey = "continuity-provider";
    try {
      const first = await runTurn({
        storePath,
        baseUrl: provider.baseUrl,
        apiKey: "sk-continuity",
        phase: 1,
        chatId,
        connectionKey,
      });
      expect(first.status).toBe("completed");
      expect(provider.requests()).toBe(1);
      // The encrypted continuation really landed in the on-disk store the
      // second process will reopen (public assistant-store port).
      const store = await NativeStore.open(storePath);
      try {
        const port = new NativeAssistantStore({ store });
        const saved = await port.continuations.getByRun(first.turnId);
        expect(JSON.stringify(saved)).toInclude("enc_continuity_1");
      } finally {
        await store.close();
      }
      // The restart: a SECOND Bun process, fresh binding-instance
      // registry, same store + same provider config + same authorized
      // credentials. It registers its own connection — the durable
      // logical identity, not a process-local id, is what the scope binds.
      const second = await runTurn({
        storePath,
        baseUrl: provider.baseUrl,
        apiKey: "sk-continuity",
        phase: 2,
        chatId,
        connectionKey,
      });
      expect(second.status).toBe("completed");
      // Exactly one new provider request — the continuation turn — and
      // the fixture witnessed the stored encrypted item replayed on it.
      expect(provider.requests()).toBe(2);
      expect(provider.lines().at(-1)).toInclude("REQUEST 2 enc=true");
    } finally {
      provider.kill();
    }
  },
  { timeout: 60_000 },
);

nativeTest(
  "credential rotation blocks continuation before transport until explicitly restored",
  async () => {
    const provider = await startFixtureProvider("sk-continuity");
    const dir = storeDir();
    const storePath = join(dir, "store.sqlite");
    const chatId = "wrong-key-chat";
    const connectionKey = "wrong-key-provider";
    try {
      const first = await runTurn({
        storePath,
        baseUrl: provider.baseUrl,
        apiKey: "sk-continuity",
        phase: 1,
        chatId,
        connectionKey,
      });
      expect(first.status).toBe("completed");
      expect(provider.requests()).toBe(1);
      // Rotation cannot silently authorize history under a new credential.
      const wrong = await runTurn({
        storePath,
        baseUrl: provider.baseUrl,
        apiKey: "sk-not-the-key",
        phase: 2,
        chatId,
        connectionKey,
      });
      expect(wrong.status).toBe("failed");
      expect(wrong.error ?? "").toInclude(
        "provider_continuation_scope_mismatch",
      );
      expect(provider.lines().some((line) => line.startsWith("DENIED "))).toBe(
        false,
      );
      expect(provider.requests()).toBe(1);
      // Explicit confirmation transfers only settled compatible history.
      const restored = await runTurn({
        storePath,
        baseUrl: provider.baseUrl,
        apiKey: "sk-continuity",
        phase: 3,
        chatId,
        connectionKey,
        restoreContinuity: {
          expectedGeneration: 3,
          previousGeneration: 1,
          confirmUnchangedScope: true,
        },
      });
      expect(restored.status).toBe("completed");
      expect(provider.requests()).toBe(2);
      expect(provider.lines().at(-1)).toInclude("REQUEST 2 enc=true");
    } finally {
      provider.kill();
    }
  },
  { timeout: 60_000 },
);

nativeTest(
  "revocation requires explicit settled-state restoration after re-registration",
  async () => {
    const provider = await startFixtureProvider("sk-continuity");
    const dir = storeDir();
    const storePath = join(dir, "store.sqlite");
    const chatId = "revoked-chat";
    const connectionKey = "revoked-provider";
    try {
      const first = await runTurn({
        storePath,
        baseUrl: provider.baseUrl,
        apiKey: "sk-continuity",
        phase: 1,
        chatId,
        connectionKey,
      });
      expect(first.status).toBe("completed");
      const again = await runTurn({
        storePath,
        baseUrl: provider.baseUrl,
        apiKey: "sk-continuity",
        phase: 2,
        chatId,
        connectionKey,
        revokeAndReRegister: true,
        restoreContinuity: {
          expectedGeneration: 2,
          previousGeneration: 1,
          confirmUnchangedScope: true,
        },
      });
      // Explicit confirmation restores history while the old generation stays fenced.
      expect(again.status).toBe("completed");
      expect(again.connectionId).toBe(first.connectionId);
      expect(provider.requests()).toBe(2);
      expect(provider.lines().at(-1)).toInclude("REQUEST 2 enc=true");
    } finally {
      provider.kill();
    }
  },
  { timeout: 60_000 },
);

nativeTest(
  "a wrong providerId still rejects the resumed turn before transport",
  async () => {
    const provider = await startFixtureProvider("sk-continuity");
    const dir = storeDir();
    const storePath = join(dir, "store.sqlite");
    const chatId = "scope-pin-chat";
    const connectionKey = "scope-pin-provider";
    try {
      const first = await runTurn({
        storePath,
        baseUrl: provider.baseUrl,
        apiKey: "sk-continuity",
        phase: 1,
        chatId,
        connectionKey,
      });
      expect(first.status).toBe("completed");
      // The durable identity does NOT weaken the provider binding: the
      // session's provider stamp is checked before anything is sent.
      const foreign = await runTurn({
        storePath,
        baseUrl: provider.baseUrl,
        apiKey: "sk-continuity",
        phase: 2,
        chatId,
        connectionKey,
        providerId: "not-continuity",
      });
      expect(foreign.status).toBe("failed");
      expect(foreign.error ?? "").toInclude("provider_binding_mismatch");
      expect(provider.requests()).toBe(1);
    } finally {
      provider.kill();
    }
  },
  { timeout: 60_000 },
);

nativeTest(
  "a continuation stored on the durable identity is still rejected by any other connection",
  async () => {
    const provider = await startFixtureProvider("sk-continuity");
    const dir = storeDir();
    const storePath = join(dir, "store.sqlite");
    const chatId = "foreign-conn-chat";
    const connectionKey = "foreign-conn-provider";
    try {
      const first = await runTurn({
        storePath,
        baseUrl: provider.baseUrl,
        apiKey: "sk-continuity",
        phase: 1,
        chatId,
        connectionKey,
      });
      expect(first.status).toBe("completed");
      // Same authorized credentials, same provider — but a DIFFERENT
      // connection (a keyless, process-local registration). The scope
      // check keeps its full strictness: wrong connection is a mismatch,
      // rejected before transport, never a silent stateless fallback.
      const foreign = await runTurn({
        storePath,
        baseUrl: provider.baseUrl,
        apiKey: "sk-continuity",
        phase: 2,
        chatId,
        connectionKey,
        foreignConnection: true,
      });
      expect(foreign.status).toBe("failed");
      expect(foreign.error ?? "").toInclude(
        "provider_continuation_scope_mismatch",
      );
      expect(provider.requests()).toBe(1);
    } finally {
      provider.kill();
    }
  },
  { timeout: 60_000 },
);
