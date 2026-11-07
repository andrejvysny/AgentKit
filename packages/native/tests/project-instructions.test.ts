import { testServiceOptions } from "./helpers/test-profiles.js";
import { expect, test } from "bun:test";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { available, missing, required } from "./helpers/binding.js";
import { startProviderMock } from "./helpers/loopback-provider.js";
import type { NativeSessionService, NativeStore } from "../src/index.js";

let NativeStoreCtor: typeof NativeStore;
let NativeSessionServiceCtor: typeof NativeSessionService;
if (required && !available) {
  throw new Error(
    `AGENTKIT_REQUIRE_NATIVE=1 but the binding is not built (missing ${missing.join(", ")}); run \`bun run build:native\` in packages/native`,
  );
}
if (available) {
  const binding = await import("../src/index.js");
  NativeStoreCtor = binding.NativeStore;
  NativeSessionServiceCtor = binding.NativeSessionService;
}

const suite = available ? test : test.skip;

function disposableDir(): string {
  return mkdtempSync(join(tmpdir(), "agentkit-instructions-"));
}

function settledStatus(update: unknown): string | undefined {
  if (update && typeof update === "object" && "task" in update) {
    const task = update.task;
    if (task && typeof task === "object" && "status" in task) {
      const status = task.status;
      if (typeof status === "string") return status;
    }
  }
  return undefined;
}

function eventOf(
  update: unknown,
): { type: string; data?: Record<string, unknown> } | null {
  if (update && typeof update === "object" && "event" in update) {
    const envelope = update.event;
    if (
      envelope &&
      typeof envelope === "object" &&
      "event" in envelope &&
      envelope.event &&
      typeof envelope.event === "object" &&
      "type" in envelope.event &&
      typeof envelope.event.type === "string"
    ) {
      const data =
        "data" in envelope.event &&
        envelope.event.data &&
        typeof envelope.event.data === "object"
          ? (envelope.event.data as Record<string, unknown>)
          : undefined;
      return { type: envelope.event.type, data };
    }
  }
  return null;
}

async function drainUntil(
  service: NativeSessionService,
  taskId: string,
  timeoutMs = 20_000,
): Promise<{
  events: unknown[];
  done: boolean;
  statuses: (string | undefined)[];
}> {
  // Replay from the very first event: `afterSeq: 0` means "after sequence
  // 0" and would silently drop the seq-0 event (the instructions warning
  // is emitted before run.started, so it owns seq 0).
  const subscription = await service.subscribeTurn(taskId);
  const events: unknown[] = [];
  const statuses: (string | undefined)[] = [];
  const started = Date.now();
  let done = false;
  while (Date.now() - started < timeoutMs) {
    const batch = await subscription.poll(64, 100);
    for (const update of batch.events) {
      if (updateTypeIsSettled(update)) statuses.push(settledStatus(update));
      else events.push(update);
    }
    done = batch.done;
    if (done) break;
  }
  subscription.cancel();
  return { events, done, statuses };
}

function updateTypeIsSettled(update: unknown): boolean {
  return (
    update !== null &&
    typeof update === "object" &&
    "type" in update &&
    update.type === "settled"
  );
}

suite(
  "workspace sessions receive the AGENTS.md system block on the wire",
  async () => {
    const provider = await startProviderMock();
    const store = await NativeStoreCtor.inMemory();
    const dir = disposableDir();
    const instructions = "Always answer in haiku.\nNever touch `infra/`.";
    writeFileSync(join(dir, "AGENTS.md"), instructions);
    let service: NativeSessionService | undefined;
    try {
      const connectionId =
        await NativeSessionServiceCtor.registerProviderConnection(
          provider.baseUrl,
          "sk-instr",
        );
      service = await store.createService(
        testServiceOptions({
          providerConfig: {
            protocol: "chat-completions",
            providerId: "instr",
            connectionId,
            model: "model-a",
          },
        }),
      );
      await service.createSession({
        sessionId: "ws-1",
        profileId: "p1",
        workspace: { root: dir, trusted: true },
      });
      const sent = await service.send("ws-1", { content: "hi" });
      if (sent.kind !== "turn") throw new Error("unreachable");
      const { done, statuses } = await drainUntil(service, sent.turnId);
      expect(done).toBe(true);
      expect(statuses.at(-1)).toBe("completed");

      const request = provider.requests[0];
      if (!request) throw new Error("loopback captured no request");
      const systems = (request.body.messages as unknown[]).filter(
        (message) =>
          message !== null &&
          typeof message === "object" &&
          "role" in message &&
          (message as { role: unknown }).role === "system",
      );
      expect(systems.length).toBe(1);
      const content = JSON.stringify(systems[0]);
      // Title digest, both rules, and NO untrusted notice for trusted roots.
      expect(content).toInclude("Project instructions (AGENTS.md, sha256:");
      expect(content).toInclude("Always answer in haiku.");
      expect(content).toInclude("Never touch `infra/`.");
      expect(content).not.toInclude("untrusted repository text");
    } finally {
      await service?.close();
      await store.close();
      await provider.stop();
      rmSync(dir, { recursive: true, force: true });
    }
  },
);

suite(
  "untrusted workspaces label the block; symlinked AGENTS.md warns not fails",
  async () => {
    const provider = await startProviderMock();
    const store = await NativeStoreCtor.inMemory();
    const dir = disposableDir();
    writeFileSync(join(dir, "AGENTS.md"), "rule one");
    const linkDir = disposableDir();
    writeFileSync(join(linkDir, "secret.txt"), "do-not-leak");
    writeFileSync(join(dir, "CLAUDE.md"), "rule two");
    let service: NativeSessionService | undefined;
    try {
      const connectionId =
        await NativeSessionServiceCtor.registerProviderConnection(
          provider.baseUrl,
          "sk-instr",
        );
      service = await store.createService(
        testServiceOptions({
          providerConfig: {
            protocol: "chat-completions",
            providerId: "instr",
            connectionId,
            model: "model-a",
          },
        }),
      );
      // Untrusted root: every section carries the verbatim notice.
      await service.createSession({
        sessionId: "ws-u",
        profileId: "p1",
        workspace: { root: dir, trusted: false },
      });
      const sent = await service.send("ws-u", { content: "hi" });
      if (sent.kind !== "turn") throw new Error("unreachable");
      const first = await drainUntil(service, sent.turnId);
      expect(first.statuses.at(-1)).toBe("completed");
      const request = provider.requests[0];
      if (!request) throw new Error("loopback captured no request");
      const content = JSON.stringify(request.body.messages);
      expect(content).toInclude("untrusted repository text");
      expect(content).toInclude("rule one");
      expect(content).toInclude("rule two");

      // Symlinked AGENTS.md: refused with a run warning, never read, and
      // the turn still settles completed.
      rmSync(join(dir, "AGENTS.md"));
      writeFileSync(join(dir, "AGENTS.md"), ""); // placeholder then relink
      rmSync(join(dir, "AGENTS.md"));
      const { symlinkSync } = await import("node:fs");
      symlinkSync(join(linkDir, "secret.txt"), join(dir, "AGENTS.md"));
      const second = await service.send("ws-u", { content: "again" });
      if (second.kind !== "turn") throw new Error("unreachable");
      const drained = await drainUntil(service, second.turnId);
      expect(drained.statuses.at(-1)).toBe("completed");
      const warnings = drained.events
        .map((update) => eventOf(update))
        .filter((event) => event?.type === "run.warning");
      const refusal = warnings.find((event) =>
        String(event?.data?.message ?? "").includes("AGENTS.md"),
      );
      expect(refusal).toBeDefined();
      const wire = JSON.stringify(provider.requests[1]?.body ?? "");
      expect(wire).not.toInclude("do-not-leak");
      // CLAUDE.md still loads alongside the refused file.
      expect(wire).toInclude("rule two");
    } finally {
      await service?.close();
      await store.close();
      await provider.stop();
      rmSync(dir, { recursive: true, force: true });
      rmSync(linkDir, { recursive: true, force: true });
    }
  },
);
