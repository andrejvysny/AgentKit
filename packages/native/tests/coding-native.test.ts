/**
 * The NATIVE coding lane through the PUBLIC binding: the Rust toolpack
 * registers through `registerCodingTools`, executes on the same executor
 * lane as JS tools, and is governed by the crate's own guards and
 * fingerprint-v4 standing approvals — no TS pack runtime anywhere on this
 * path.
 *
 * Contract checked here:
 *  1. descriptor parity: the descriptors the provider actually receives
 *     from the native lane match the frozen TS authority,
 *     descriptor-by-descriptor (names, descriptions, input schemas) —
 *     `fixtures/coding-descriptors.json`, frozen from the TypeScript pack
 *     at its deletion, is the differential oracle;
 *  2. behavior: writes and reads through the native tools are the crate's
 *     real files, with the crate's real results;
 *  3. approval lifecycle through the binding: first bash command → durable
 *     approval request carrying the v4 standing key; approve → executes;
 *     identical rerun with no state change → executes WITHOUT a new
 *     approval; mutate a bound direct startup input → NEW approval required
 *     with ZERO execution and ZERO receipts (harmless counter file + the
 *     durable receipts table are the witnesses);
 *  4. process-group cancellation: cancelling a turn takes the native
 *     command's whole group down, not just the turn row.
 */
import { expect, test } from "bun:test";
import { mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
// Differential oracle ONLY (contract item 1): the frozen descriptor fixture
// (TS authority, frozen 2026-11-02 at the toolpacks-coding deletion) stays
// for exactly this comparison — no native path imports the retired pack.
import descriptors from "./fixtures/coding-descriptors.json";
import { NativeSessionService, NativeStore } from "../src/index.js";
import { available } from "./helpers/binding.js";

const nativeTest = available ? test : test.skip;

/** The host governance annotation prepended to advertised DirectWrite-lane
 * write tools (`rust/crates/host/src/governance/staging.rs`). */
const DIRECT_WRITE_PREFIX =
  "Direct write tool; host authorization and domain receipts are required. ";

type Step =
  | { kind: "content"; text: string }
  | { kind: "toolCall"; name: string; args: Record<string, unknown> };

interface Served {
  baseUrl: string;
  /** Parsed chat-completions bodies, in order, across every turn. */
  requests: Record<string, unknown>[];
  stop(): Promise<void>;
}

/** Scripted chat-completions SSE server; step `i` answers provider call `i`
 * (the same shape `cancellation.test.ts` drives the Rust provider lane with). */
async function serveProvider(script: Step[]): Promise<Served> {
  const requests: Record<string, unknown>[] = [];
  const server = Bun.serve({
    hostname: "127.0.0.1",
    port: 0,
    async fetch(request) {
      const url = new URL(request.url);
      if (
        request.method !== "POST" ||
        !url.pathname.endsWith("/chat/completions")
      )
        return new Response("not found", { status: 404 });
      requests.push((await request.json()) as Record<string, unknown>);
      const step = script[Math.min(requests.length - 1, script.length - 1)]!;
      const chunks: string[] = [];
      const push = (delta: Record<string, unknown>, finish: string | null) =>
        chunks.push(
          `data: ${JSON.stringify({
            id: `chatcmpl-${requests.length}-${chunks.length + 1}`,
            object: "chat.completion.chunk",
            created: 1,
            model: "loopback-model",
            choices: [{ index: 0, delta, finish_reason: finish }],
          })}\n\n`,
        );
      push({ role: "assistant" }, null);
      if (step.kind === "content") push({ content: step.text }, "stop");
      if (step.kind === "toolCall") {
        push(
          {
            tool_calls: [
              {
                index: 0,
                id: `call-${requests.length}`,
                type: "function",
                function: {
                  name: step.name,
                  arguments: JSON.stringify(step.args),
                },
              },
            ],
          },
          "tool_calls",
        );
      }
      const stream = new ReadableStream<Uint8Array>({
        start(controller) {
          const encoder = new TextEncoder();
          for (const chunk of chunks) controller.enqueue(encoder.encode(chunk));
          controller.enqueue(encoder.encode("data: [DONE]\n\n"));
          controller.close();
        },
      });
      return new Response(stream, {
        headers: { "content-type": "text/event-stream" },
      });
    },
  });
  return {
    baseUrl: `http://127.0.0.1:${server.port}/v1`,
    requests,
    stop: () => Promise.resolve(server.stop(true)),
  };
}

type Mode = "read_only" | "ask" | "auto_edit" | "full_auto" | "plan";

async function openNativeCoding(options: {
  workspace: string;
  script: Step[];
  codingMode: Mode;
  testCommands?: string[];
}) {
  const provider = await serveProvider(options.script);
  const dir = mkdtempSync(join(tmpdir(), "agentkit-coding-native-"));
  const store = await NativeStore.open(join(dir, "store.sqlite"));
  const connectionId = await NativeSessionService.registerProviderConnection(
    provider.baseUrl,
    null,
  );
  const service = await store.createService({
    jsToolAuthority: { configuredMode: options.codingMode },
    providerConfig: {
      protocol: "chat-completions",
      providerId: "probe",
      connectionId,
      model: "loopback-model",
    },
  });
  await service.registerCodingTools({
    workspaceRoot: options.workspace,
    // Keep remembered-approval controls independent of personal startup
    // configuration while executing the real Bun and Node from PATH.
    environment: {
      PATH: process.env.PATH ?? "/usr/bin:/bin",
      HOME: options.workspace,
    },
    sandbox: "off",
    testCommands: options.testCommands ?? ["bun test"],
    configuredMode: options.codingMode,
    planToolNames: ["submit_plan"],
  });
  const session = await service.createSession({ profileId: "coding" });
  // The created-session projection: id (port shape) or sessionId.
  const sessionId =
    typeof session.id === "string"
      ? session.id
      : "sessionId" in session && typeof session.sessionId === "string"
        ? session.sessionId
        : (() => {
            throw new Error(
              `created session carries no id: ${JSON.stringify(session)}`,
            );
          })();
  return { provider, store, service, sessionId, dir };
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
  pollMs = 25,
): Promise<boolean> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (await condition()) return true;
    await Bun.sleep(pollMs);
  }
  return false;
}

interface ParkedApproval {
  requestId: string;
  answerId: string;
  content: Record<string, unknown>;
  toolName?: string;
}

/** The approval the durable projection says the session is parked on. */
async function parkedApproval(
  service: NativeSessionService,
  sessionId: string,
): Promise<ParkedApproval | undefined> {
  const state = (await service.getState(sessionId)) as Record<string, unknown>;
  const waiting = state.waiting as
    | { inputId?: string; content?: Record<string, unknown> }
    | null
    | undefined;
  if (!waiting?.inputId) return undefined;
  const approval = state.waitingApproval as
    | { toolName?: string }
    | null
    | undefined;
  return {
    requestId: waiting.inputId,
    answerId: `ans:${waiting.inputId}`,
    content: waiting.content ?? {},
    ...(approval?.toolName === undefined
      ? {}
      : { toolName: approval.toolName }),
  };
}

function receiptsFor(
  store: NativeStore,
  runId: string,
): Promise<Record<string, unknown>[]> {
  return store.callStore<Record<string, unknown>[]>(
    "toolExecutions.listByRun",
    { runId },
  );
}

function counterBytes(path: string): number {
  try {
    return readFileSync(path).length;
  } catch {
    return 0;
  }
}

/**
 * The `bun test` witness workspace: `bun test` (the exact command shape the
 * authorization suite proves is standing-keyable) runs the REAL bun, which
 * runs `tick.test.ts` — the only thing that touches the counter file, so a
 * counter byte is direct evidence that the command genuinely executed.
 * package.json's `test` script (`node check.mjs`) is what the fingerprint
 * parses into the directly-selected startup input (baseline
 * `bindScriptEntrypoints` → `directScriptFiles("node check.mjs")`):
 * `check.mjs` is the bound file — mutating it must force a fresh ask.
 */
function counterWorkspace(options: { counter: string }): string {
  const root = mkdtempSync(join(tmpdir(), "agentkit-coding-native-ws-"));
  writeFileSync(
    join(root, "package.json"),
    '{"name":"probe","version":"1.0.0","scripts":{"test":"node check.mjs"}}',
  );
  writeFileSync(join(root, "check.mjs"), "console.log(1)\n");
  writeFileSync(
    join(root, "tick.test.ts"),
    [
      'import { test } from "bun:test";',
      'import { appendFileSync } from "node:fs";',
      `test("tick", () => { appendFileSync(${JSON.stringify(options.counter)}, "x"); });`,
      "",
    ].join("\n"),
  );
  return root;
}

// ---------------------------------------------------------------------------
// 1. Descriptor parity: provider-visible descriptors vs the frozen TS
//    authority (`fixtures/coding-descriptors.json`, frozen 2026-11-02).
// ---------------------------------------------------------------------------

nativeTest(
  "the native lane advertises the frozen pack descriptors, descriptor-by-descriptor",
  async () => {
    const workspace = mkdtempSync(join(tmpdir(), "agentkit-coding-native-d-"));
    const { provider, store, service, sessionId } = await openNativeCoding({
      workspace,
      script: [{ kind: "content", text: "nothing to do" }],
      codingMode: "full_auto",
    });
    try {
      const sent = await service.send(sessionId, { content: "hello" });
      const turnId = String((sent as Record<string, unknown>).turnId ?? "");
      expect(
        await waitFor(async () => {
          const status = await turnStatus(service, sessionId, turnId);
          return status === "completed" || status === "failed";
        }, 20_000),
      ).toBe(true);
      expect(provider.requests.length).toBeGreaterThanOrEqual(1);

      type PayloadTool = {
        type?: string;
        function?: {
          name: string;
          description?: string;
          parameters?: unknown;
        };
      };
      const nativeTools = (provider.requests[0]!.tools ?? []) as PayloadTool[];
      const native = new Map<
        string,
        { description?: string; parameters?: unknown }
      >(
        nativeTools.map((tool) => [
          tool.function?.name ?? "",
          tool.function ?? {},
        ]),
      );

      // The frozen authority, re-shaped to the pack's tool envelope so the
      // comparison below is the differential exactly as it ran against TS.
      const tsTools = descriptors.tools.map((tool) => ({
        definition: {
          name: tool.name,
          effect: tool.effect,
          description: tool.description ?? undefined,
          inputSchema: tool.inputSchema,
        },
      }));

      // The same SET of tools — nothing more, nothing less.
      expect(new Set(native.keys())).toEqual(
        new Set(tsTools.map((tool) => tool.definition.name)),
      );
      expect(tsTools.length).toBe(11);

      for (const tool of tsTools) {
        const name = tool.definition.name;
        const advertised = native.get(name);
        expect(advertised, `native lane advertises ${name}`).toBeDefined();
        const entry = nativeTools.find(
          (candidate) => candidate.function?.name === name,
        )!;
        expect(entry.type, `${name} is an advertised function`).toBe(
          "function",
        );
        // Descriptions agree on the same words (an absent description on
        // one side is an absent description on the other). The one legal
        // addition is the HOST's honest annotation on DirectWrite-lane
        // write tools (rust/crates/host governance/staging.rs): the base
        // text must still be byte-identical to the pack's.
        const directWrite =
          tool.definition.effect === "write" &&
          (advertised!.description ?? "").startsWith(DIRECT_WRITE_PREFIX);
        expect(directWrite, `${name} write-lane annotation`).toBe(
          tool.definition.effect === "write",
        );
        expect(
          (advertised!.description ?? "").slice(
            directWrite ? DIRECT_WRITE_PREFIX.length : 0,
          ) || undefined,
          `${name} description`,
        ).toEqual(tool.definition.description ?? undefined);
        // The model-facing schema is the pack's schema, structurally
        // identical — the same tool contract the TS pack produced.
        expect(advertised!.parameters, `${name} input schema`).toEqual(
          tool.definition.inputSchema,
        );
      }
    } finally {
      await service.close().catch(() => undefined);
      await store.close();
      await provider.stop();
    }
  },
  { timeout: 60_000 },
);

// ---------------------------------------------------------------------------
// 2. Behavior: native writes and reads are the crate's real files/results.
// ---------------------------------------------------------------------------

nativeTest(
  "native write_file and read_file behave like the pack (real files, model-visible results)",
  async () => {
    const workspace = mkdtempSync(join(tmpdir(), "agentkit-coding-native-b-"));
    const { provider, store, service, sessionId } = await openNativeCoding({
      workspace,
      script: [
        {
          kind: "toolCall",
          name: "write_file",
          args: { path: "notes/hello.txt", content: "hello native" },
        },
        {
          kind: "toolCall",
          name: "read_file",
          args: { path: "notes/hello.txt" },
        },
        { kind: "content", text: "done" },
      ],
      codingMode: "full_auto",
    });
    try {
      const sent = await service.send(sessionId, { content: "write and read" });
      const turnId = String((sent as Record<string, unknown>).turnId ?? "");
      expect(
        await waitFor(async () => {
          const status = await turnStatus(service, sessionId, turnId);
          return status === "completed" || status === "failed";
        }, 20_000),
      ).toBe(true);
      expect(await turnStatus(service, sessionId, turnId)).toBe("completed");
      expect(readFileSync(join(workspace, "notes", "hello.txt"), "utf8")).toBe(
        "hello native",
      );
      // The read result reached the MODEL, not just the disk: the third
      // provider request carries the tool output.
      expect(provider.requests.length).toBe(3);
      const carried = JSON.stringify(provider.requests[2]!.messages);
      expect(carried).toContain("hello native");
      // Durable execution receipts: the write is a proven completed effect.
      const receipts = await receiptsFor(store, turnId);
      expect(receipts.some((r) => r.toolName === "write_file")).toBe(true);
    } finally {
      await service.close().catch(() => undefined);
      await store.close();
      await provider.stop();
    }
  },
  { timeout: 60_000 },
);

// ---------------------------------------------------------------------------
// 3a. Counter witness in full_auto: native commands RUN, without asking.
// ---------------------------------------------------------------------------

nativeTest(
  "full_auto native run_command executes without approval (harmless counter witness)",
  async () => {
    const scratch = mkdtempSync(join(tmpdir(), "agentkit-coding-native-c-"));
    const counter = join(scratch, "counter.bin");
    const root = counterWorkspace({ counter });
    const { provider, store, service, sessionId } = await openNativeCoding({
      workspace: root,
      script: [
        {
          kind: "toolCall",
          name: "run_command",
          args: { command: "bun test" },
        },
        { kind: "content", text: "green" },
      ],
      codingMode: "full_auto",
    });
    try {
      const sent = await service.send(sessionId, { content: "run the tests" });
      const turnId = String((sent as Record<string, unknown>).turnId ?? "");
      expect(
        await waitFor(async () => {
          const status = await turnStatus(service, sessionId, turnId);
          return status === "completed" || status === "failed";
        }, 60_000),
      ).toBe(true);
      expect(await turnStatus(service, sessionId, turnId)).toBe("completed");
      expect(counterBytes(counter), "the stub really ran").toBe(1);
      // And it never asked.
      expect(await parkedApproval(service, sessionId)).toBeUndefined();
      const receipts = await receiptsFor(store, turnId);
      expect(receipts.some((r) => r.toolName === "run_command")).toBe(true);
    } finally {
      await service.close().catch(() => undefined);
      await store.close();
      await provider.stop();
    }
  },
  { timeout: 90_000 },
);

// ---------------------------------------------------------------------------
// 3b. Approval lifecycle through the binding, including the deferred
//     acceptance: mutate a bound direct input → fresh ask, ZERO execution,
//     ZERO receipts.
// ---------------------------------------------------------------------------

nativeTest(
  "ask mode: durable approval with the v4 standing key; approve→run; identical rerun runs WITHOUT asking; mutating the bound input re-asks with zero execution and zero receipts",
  async () => {
    const scratch = mkdtempSync(join(tmpdir(), "agentkit-coding-native-a-"));
    const counter = join(scratch, "counter.bin");
    const root = counterWorkspace({ counter });
    const command = { command: "bun test" };
    const { provider, store, service, sessionId } = await openNativeCoding({
      workspace: root,
      script: [
        // turn 1: first ask, then the post-approval summary.
        { kind: "toolCall", name: "run_command", args: command },
        { kind: "content", text: "first done" },
        // turn 2: identical rerun — must execute under the standing grant.
        { kind: "toolCall", name: "run_command", args: command },
        { kind: "content", text: "second done" },
        // turn 3 (after check.mjs mutation): fresh ask, then its approval.
        { kind: "toolCall", name: "run_command", args: command },
        { kind: "content", text: "third done" },
      ],
      codingMode: "ask",
    });
    try {
      // ---- first command: a DURABLE approval request with the standing key.
      const turn1 = String(
        (
          (await service.send(sessionId, {
            content: "run the tests",
          })) as Record<string, unknown>
        ).turnId ?? "",
      );
      expect(
        await waitFor(
          async () =>
            (await turnStatus(service, sessionId, turn1)) ===
            "waiting_approval",
          30_000,
        ),
      ).toBe(true);
      expect(counterBytes(counter), "parked before any effect").toBe(0);
      const ask1 = await parkedApproval(service, sessionId);
      expect(ask1).toBeDefined();
      expect(ask1!.toolName).toBe("run_command");
      expect(ask1!.content.kind).toBe("approval");
      expect(String(ask1!.content.argsSha256 ?? "")).toMatch(/^[0-9a-f]{64}$/);
      const digest1 = String(
        (ask1!.content.standing as Record<string, unknown> | undefined)
          ?.digest ?? "",
      );
      expect(
        digest1,
        "a keyable ordinary command carries the standing key",
      ).toMatch(/^[0-9a-f]{64}$/);
      // No receipt may exist while parked (the park precedes admission).
      expect(await receiptsFor(store, turn1)).toEqual([]);

      // ---- approve WITH remembering: the command runs once.
      await service.approve(ask1!.requestId, ask1!.answerId, undefined, true);
      expect(
        await waitFor(async () => {
          const status = await turnStatus(service, sessionId, turn1);
          return status === "completed" || status === "failed";
        }, 30_000),
      ).toBe(true);
      expect(await turnStatus(service, sessionId, turn1)).toBe("completed");
      expect(counterBytes(counter), "the approval executed once").toBe(1);
      // The remembered grant is durable state on the session, not memory.
      const chat = await store.callStore<{
        metadata?: Record<string, unknown>;
      }>("conversations.getChat", { chatId: sessionId });
      const grants = (
        chat?.metadata?.["agentkit.session.approvals"] as
          | { grants?: unknown[] }
          | undefined
      )?.grants;
      expect(Array.isArray(grants) && grants.length).toBe(1);

      // ---- identical rerun, nothing changed: executes WITHOUT a new ask.
      let sawAsk = false;
      const turn2 = String(
        (
          (await service.send(sessionId, {
            content: "run them again",
          })) as Record<string, unknown>
        ).turnId ?? "",
      );
      expect(
        await waitFor(
          async () => {
            if (await parkedApproval(service, sessionId)) sawAsk = true;
            const status = await turnStatus(service, sessionId, turn2);
            return status === "completed" || status === "failed";
          },
          30_000,
          10,
        ),
      ).toBe(true);
      expect(sawAsk, "the standing grant replaces the second ask").toBe(false);
      expect(await turnStatus(service, sessionId, turn2)).toBe("completed");
      expect(counterBytes(counter), "the standing grant executed again").toBe(
        2,
      );

      // ---- mutate the directly-bound startup input: the grant must not
      //      authorize the changed identity — fresh ask, ZERO execution,
      //      ZERO receipts.
      writeFileSync(join(root, "check.mjs"), "console.log(2)\n");
      const turn3 = String(
        (
          (await service.send(sessionId, {
            content: "and again",
          })) as Record<string, unknown>
        ).turnId ?? "",
      );
      expect(
        await waitFor(
          async () =>
            (await turnStatus(service, sessionId, turn3)) ===
            "waiting_approval",
          30_000,
        ),
      ).toBe(true);
      expect(
        counterBytes(counter),
        "a mutated bound input must not execute under the old approval",
      ).toBe(2);
      const receipts3 = await receiptsFor(store, turn3);
      expect(receipts3, "zero receipts for the parked call").toEqual([]);
      const ask3 = await parkedApproval(service, sessionId);
      expect(ask3).toBeDefined();
      const digest3 = String(
        (ask3!.content.standing as Record<string, unknown> | undefined)
          ?.digest ?? "",
      );
      expect(digest3).toMatch(/^[0-9a-f]{64}$/);
      expect(digest3, "the new key binds the mutated input").not.toBe(digest1);

      // ---- the fresh approval DOES execute (the counter is not a dead
      //      letter; the ask is live, not a stall).
      await service.approve(ask3!.requestId, ask3!.answerId, undefined, true);
      expect(
        await waitFor(async () => {
          const status = await turnStatus(service, sessionId, turn3);
          return status === "completed" || status === "failed";
        }, 30_000),
      ).toBe(true);
      expect(counterBytes(counter), "the fresh approval executed").toBe(3);
      const receipts3final = await receiptsFor(store, turn3);
      expect(
        receipts3final.some((r) => r.toolName === "run_command"),
        "the executed call left its receipt",
      ).toBe(true);
    } finally {
      await service.close().catch(() => undefined);
      await store.close();
      await provider.stop();
    }
  },
  { timeout: 120_000 },
);

// ---------------------------------------------------------------------------
// 4. Process-group cancellation on the NATIVE lane.
// ---------------------------------------------------------------------------

nativeTest(
  "cancelling a turn takes the native command's process group down",
  async () => {
    const workspace = mkdtempSync(join(tmpdir(), "agentkit-coding-native-x-"));
    const marker = `native-cancel-sleep-${process.pid}-${Date.now()}`;
    // The command keeps TWO group members whose argv carries the marker:
    // the bash leader (the `wait` keeps it from exec-optimising the command
    // text away) and the `tail -f` on a marker-named file. A pid-only kill
    // would leave the tail alive, so the witness genuinely proves the whole
    // process group went down.
    const markerFile = join(workspace, `${marker}.log`);
    writeFileSync(markerFile, "wake\n");
    Bun.spawn(["pkill", "-9", "-f", "native-cancel-sleep-"], {
      stdout: "ignore",
      stderr: "ignore",
    });
    const { provider, store, service, sessionId } = await openNativeCoding({
      workspace,
      script: [
        {
          kind: "toolCall",
          name: "run_command",
          args: {
            command: `tail -f ${markerFile} & wait`,
            timeoutSec: 40,
          },
        },
        { kind: "content", text: "unreachable" },
      ],
      codingMode: "full_auto",
    });
    try {
      const sent = await service.send(sessionId, { content: "sleep please" });
      const turnId = String((sent as Record<string, unknown>).turnId ?? "");
      const sleepRunning = async () => {
        const probe = Bun.spawn(["pgrep", "-f", marker], {
          stdout: "ignore",
          stderr: "ignore",
        });
        return (await probe.exited) === 0;
      };
      expect(
        await waitFor(sleepRunning, 15_000),
        "the native command started",
      ).toBe(true);
      const t0 = Date.now();
      await service.cancel(sessionId, turnId);
      expect(
        await waitFor(async () => !(await sleepRunning()), 8_000),
        "the whole process group is gone after cancel",
      ).toBe(true);
      expect(Date.now() - t0).toBeLessThan(10_000);
      expect(
        await waitFor(async () => {
          const status = await turnStatus(service, sessionId, turnId);
          return status === "cancelled" || status === "failed";
        }, 5_000),
      ).toBe(true);
    } finally {
      Bun.spawn(["pkill", "-f", marker], {
        stdout: "ignore",
        stderr: "ignore",
      });
      await service.close().catch(() => undefined);
      await store.close();
      await provider.stop();
    }
  },
  { timeout: 60_000 },
);
