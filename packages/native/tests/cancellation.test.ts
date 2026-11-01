/**
 * One invocation identity/cancellation/authority contract for JS-tool lanes.
 *
 * The Rust core owns the clock: when a run is cancelled, the pending tool
 * call is settled as cancelled AND the JS side must be told (the bridge
 * delivers `{"kind":"cancel","callId":...}`). The router must turn that into
 * an abort on the signal it hands the user function, so consumer-owned
 * effects (the TS coding pack's spawned process groups) receive cancellation
 * and stop. Before this contract existed the user promise kept running
 * unobserved after the native side had already given up on it.
 */
import { expect, test } from "bun:test";
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createCodingToolPack } from "@agentkit/toolpacks-coding";
import { NativeSessionService, NativeStore } from "../src/index.js";
import { available } from "./helpers/binding.js";

const nativeTest = available ? test : test.skip;

interface Served {
  baseUrl: string;
  requests: Record<string, unknown>[];
  providerAborts: number;
  stop(): Promise<void>;
}

/**
 * Scripted chat-completions SSE server. `script[i]` answers request i:
 * content deltas, or one complete tool call, or `hold` which streams slow
 * deltas forever until the connection is aborted.
 */
async function serveProvider(
  script: (
    | { kind: "content"; text: string }
    | { kind: "toolCall"; name: string; args: Record<string, unknown> }
    | { kind: "hold" }
  )[],
): Promise<Served> {
  const requests: Record<string, unknown>[] = [];
  let providerAborts = 0;
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
            id: `chatcmpl-${chunks.length + 1}`,
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
        async start(controller) {
          const encoder = new TextEncoder();
          for (const chunk of chunks) controller.enqueue(encoder.encode(chunk));
          if (step.kind === "hold") {
            request.signal.addEventListener("abort", () => {
              providerAborts += 1;
              controller.close();
            });
            try {
              for (;;) {
                await Bun.sleep(30);
                controller.enqueue(
                  encoder.encode(
                    `data: ${JSON.stringify({
                      id: "chatcmpl-hold",
                      object: "chat.completion.chunk",
                      created: 1,
                      model: "loopback-model",
                      choices: [{ index: 0, delta: { content: "…" }, finish_reason: null }],
                    })}\n\n`,
                  ),
                );
              }
            } catch {
              /* closed by abort */
            }
            return;
          }
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
    get providerAborts() {
      return providerAborts;
    },
    stop: () => Promise.resolve(server.stop(true)),
  };
}

async function openService(
  provider: Served,
  jsToolMode: "read_only" | "ask" | "auto_edit" | "full_auto" | "plan" = "full_auto",
) {
  const store = await NativeStore.open(
    join(mkdtempSync(join(tmpdir(), "agentkit-cancel-")), "store.sqlite"),
  );
  const connectionId = await NativeSessionService.registerProviderConnection(
    provider.baseUrl,
    null,
  );
  const service = await store.createService({
    jsToolAuthority: { configuredMode: jsToolMode },
    providerConfig: {
      protocol: "chat-completions",
      providerId: "probe",
      connectionId,
      model: "loopback-model",
    },
  });
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

nativeTest(
  "a cancelled tool call aborts the JS tool signal and terminalizes the turn",
    async () => {
    const provider = await serveProvider([
      { kind: "toolCall", name: "probe_block", args: {} },
      { kind: "content", text: "unreachable" },
    ]);
    const { store, service } = await openService(provider);
    try {
      let sawAbort = false;
      let observedAttemptId: string | null | undefined;
      let toolStarted = false;
      await service.registerTool(
        {
          name: "probe_block",
          description: "blocks until the invocation is cancelled",
          inputSchema: { type: "object", properties: {} },
        },
        async (_input, request) => {
          observedAttemptId = request.attemptId;
          toolStarted = true;
          const signal = request.signal;
          if (signal) {
            if (await waitFor(() => signal.aborted, 10_000)) sawAbort = true;
          }
          return { data: "tool-end" };
        },
      );
      const session = await service.createSession({ profileId: "coding" });
      const sessionId = String(session.id ?? session.sessionId);
      const sent = await service.send(sessionId, { content: "block please" });
      const turnId = String((sent as Record<string, unknown>).turnId ?? "");
      if (!(await waitFor(() => toolStarted, 15_000)))
        throw new Error("the JS tool never started");
      expect(observedAttemptId).toBeString(); // attempt identity is carried
      const t0 = Date.now();
      await service.cancel(sessionId, turnId);
      // The abort must reach user code: the router turns the bridge cancel
      // into signal abort.
      expect(await waitFor(() => sawAbort, 5_000)).toBe(true);
      // And the durable turn must terminalize promptly.
      expect(
        await waitFor(async () => {
          const status = await turnStatus(service, sessionId, turnId);
          return status === "cancelled" || status === "failed";
        }, 5_000),
      ).toBe(true);
      expect(Date.now() - t0).toBeLessThan(8_000);
    } finally {
      await service.close().catch(() => undefined);
      await store.close();
      await provider.stop();
    }
  },
  { timeout: 30_000 },
);

nativeTest(
  "cancelling a held provider stream terminalizes without waiting for the stream",
    async () => {
    const provider = await serveProvider([{ kind: "hold" }]);
    const { store, service } = await openService(provider);
    try {
      const session = await service.createSession({ profileId: "coding" });
      const sessionId = String(session.id ?? session.sessionId);
      const sent = await service.send(sessionId, { content: "stream forever" });
      const turnId = String((sent as Record<string, unknown>).turnId ?? "");
      await Bun.sleep(400); // let deltas flow
      const t0 = Date.now();
      await service.cancel(sessionId, turnId);
      expect(
        await waitFor(async () => {
          const status = await turnStatus(service, sessionId, turnId);
          return status === "cancelled" || status === "failed";
        }, 5_000),
      ).toBe(true);
      expect(Date.now() - t0).toBeLessThan(5_000);
      // The provider connection must be torn down, not left streaming.
      expect(
        await waitFor(() => provider.providerAborts > 0, 5_000),
      ).toBe(true);
    } finally {
      await service.close().catch(() => undefined);
      await store.close();
      await provider.stop();
    }
  },
  { timeout: 30_000 },
);

nativeTest(
  "a tool that resolves after cancellation cannot rewrite the terminal outcome",
    async () => {
    const provider = await serveProvider([
      { kind: "toolCall", name: "probe_stubborn", args: {} },
      { kind: "content", text: "unreachable" },
    ]);
    const { store, service } = await openService(provider);
    try {
      let releaseResolve!: () => void;
      const release = new Promise<void>((resolve) => {
        releaseResolve = resolve;
      });
      await service.registerTool(
        {
          name: "probe_stubborn",
          description: "ignores abort and succeeds late",
          inputSchema: { type: "object", properties: {} },
        },
        async () => {
          // Deliberately ignores request.signal: a consumer that fails to
          // honor the contract must still not be able to fake a success.
          await release;
          return { data: "pretend-success" };
        },
      );
      const session = await service.createSession({ profileId: "coding" });
      const sessionId = String(session.id ?? session.sessionId);
      const sent = await service.send(sessionId, { content: "stubborn" });
      const turnId = String((sent as Record<string, unknown>).turnId ?? "");
      await Bun.sleep(400); // dispatch happened
      await service.cancel(sessionId, turnId);
      expect(
        await waitFor(async () => {
          const status = await turnStatus(service, sessionId, turnId);
          return status === "cancelled" || status === "failed";
        }, 5_000),
      ).toBe(true);
      // Late settle arrives AFTER the terminal: it must be ignored.
      releaseResolve();
      await Bun.sleep(500);
      const status = await turnStatus(service, sessionId, turnId);
      expect(status === "cancelled" || status === "failed").toBe(true);
      const turns = (await service.listTurns(sessionId)) as {
        turnId?: string;
        status?: string;
      }[];
      expect(
        turns.find((t) => t.turnId === turnId)?.status === "completed",
      ).toBe(false);
    } finally {
      await service.close().catch(() => undefined);
      await store.close();
      await provider.stop();
    }
  },
  { timeout: 40_000 },
);

nativeTest(
  "a coding-owned process receives cancellation and stops",
    async () => {
    const workspace = mkdtempSync(join(tmpdir(), "agentkit-cancel-ws-"));
    // Per-run marker: a constant marker lets a survivor of a previous run
    // (or a parallel test) satisfy the pgrep witnesses and race the cancel
    // before this run's dispatch.
    const marker = `cancel-probe-sleep-marker-${process.pid}-${Date.now()}`;
    Bun.spawn(["pkill", "-9", "-f", "cancel-probe-sleep-marker-"], {
      stdout: "ignore",
      stderr: "ignore",
    });
    const provider = await serveProvider([
      {
        kind: "toolCall",
        name: "run_command",
        args: { command: `sleep 45 # ${marker}`, timeoutSec: 40 },
      },
      { kind: "content", text: "done" },
    ]);
    const { store, service } = await openService(provider);
    const pack = createCodingToolPack({
      workspace: { root: workspace },
      testCommands: [],
      sandbox: "off",
    });
    try {
      const tools = await pack.contribute({
        bindings: [],
        limits: { profile: "medium", maxBytes: 256 * 1024, maxItems: 1024 },
      });
      const runCommand = tools.find((tool) => tool.definition.name === "run_command");
      if (!runCommand) throw new Error("run_command missing from pack");
      await service.registerTool(
        runCommand.definition as never,
        (input, request) =>
          runCommand.execute(
            {
              runId: request.runId,
              bindings: [],
              limits: { profile: "medium", maxBytes: 256 * 1024, maxItems: 1024 },
              ...(request.signal ? { signal: request.signal } : {}),
            } as never,
            input,
          ),
      );
      const session = await service.createSession({ profileId: "coding" });
      const sessionId = String(session.id ?? session.sessionId);
      const sent = await service.send(sessionId, { content: "sleep please" });
      const turnId = String((sent as Record<string, unknown>).turnId ?? "");
      const sleepRunning = async () => {
        const probe = Bun.spawn(["pgrep", "-f", marker], { stdout: "ignore", stderr: "ignore" });
        return (await probe.exited) === 0;
      };
      expect(await waitFor(sleepRunning, 15_000)).toBe(true); // process started
      const t0 = Date.now();
      await service.cancel(sessionId, turnId);
      // The durable turn terminalizes AND the spawned process group is gone:
      // the cancel message must reach the tool callback and its abort must
      // kill the group, not leave `sleep 45` running.
      expect(await waitFor(async () => !(await sleepRunning()), 8_000)).toBe(true);
      expect(Date.now() - t0).toBeLessThan(10_000);
      expect(
        await waitFor(async () => {
          const status = await turnStatus(service, sessionId, turnId);
          return status === "cancelled" || status === "failed";
        }, 5_000),
      ).toBe(true);
    } finally {
      Bun.spawn(["pkill", "-f", marker], { stdout: "ignore", stderr: "ignore" });
      await service.close().catch(() => undefined);
      await store.close();
      await provider.stop();
    }
  },
  { timeout: 60_000 },
);
