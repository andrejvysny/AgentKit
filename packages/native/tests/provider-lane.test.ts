import { expect, test } from "bun:test";

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

function updateType(update: unknown): string {
  if (update && typeof update === "object" && "type" in update) {
    const kind = update.type;
    if (typeof kind === "string") return kind;
  }
  return "";
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

function eventOf(update: unknown): {
  type: string;
  data?: Record<string, unknown>;
} | null {
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
  const subscription = await service.subscribeTurn(taskId, 0);
  const events: unknown[] = [];
  const statuses: (string | undefined)[] = [];
  const started = Date.now();
  let done = false;
  while (Date.now() - started < timeoutMs) {
    const batch = await subscription.poll(64, 100);
    for (const update of batch.events) {
      if (updateType(update) === "settled")
        statuses.push(settledStatus(update));
      else events.push(update);
    }
    done = batch.done;
    if (done) break;
  }
  subscription.cancel();
  return { events, done, statuses };
}

function kindsOf(events: unknown[]): string[] {
  const kinds: string[] = [];
  for (const update of events) {
    const event = eventOf(update);
    if (event) kinds.push(event.type);
  }
  return kinds;
}

suite(
  "rust provider lane (chat-completions): streams real deltas through the binding",
  async () => {
    const provider = await startProviderMock();
    const store = await NativeStoreCtor.inMemory();
    let service: NativeSessionService | undefined;
    try {
      const connectionId =
        await NativeSessionServiceCtor.registerProviderConnection(
          provider.baseUrl,
          "sk-loopback",
        );
      expect(connectionId).toStartWith("conn_");
      service = await store.createService({
        providerConfig: {
          protocol: "chat-completions",
          providerId: "loopback",
          connectionId,
          model: "model-a",
          temperature: 0.3,
          maxTokens: 128,
        },
      });
      // Deliberately no setProvider: the JS callback slot stays unused.
      await service.createSession({ sessionId: "lane-cc", profileId: "p1" });
      const sent = await service.send("lane-cc", { content: "say hi" });
      expect(sent.kind).toBe("turn");
      if (sent.kind !== "turn") throw new Error("unreachable");

      const { events, done, statuses } = await drainUntil(service, sent.turnId);
      expect(done).toBe(true);
      expect(statuses.at(-1)).toBe("completed");

      // The Rust lane streams every provider delta verbatim; the JS lane
      // collapses the same provider answer into a single synthesized chunk.
      const kinds = kindsOf(events);
      expect(kinds.filter((k) => k === "run.message.delta")).toHaveLength(3);
      expect(
        events
          .map((u) => eventOf(u))
          .filter((e) => e?.type === "run.message.delta")
          .map((e) => e?.data?.delta),
      ).toEqual(["Hel", "lo", " world"]);
      const reasoning = events
        .map((u) => eventOf(u))
        .find((e) => e?.type === "run.message.reasoning-delta");
      expect(reasoning?.data?.delta).toBe("thinking");
      const usage = events
        .map((u) => eventOf(u))
        .find((e) => e?.type === "run.usage");
      expect(usage?.data).toMatchObject({
        promptTokens: 9,
        completionTokens: 3,
        totalTokens: 12,
        cachedPromptTokens: 3,
        reasoningTokens: 1,
      });
      expect(kinds).toContain("run.completed");

      // Wire fidelity: what the Rust transport actually put on the socket.
      expect(provider.requests).toHaveLength(1);
      const request = provider.requests[0];
      if (!request) throw new Error("loopback captured no request");
      expect(request.path).toBe("/chat/completions");
      expect(request.headers.authorization).toBe("Bearer sk-loopback");
      expect(request.headers["content-type"]).toInclude("application/json");
      expect(request.headers.accept).toInclude("application/json");
      expect(request.body.model).toBe("model-a");
      expect(request.body.stream).toBe(true);
      expect(request.body.stream_options).toEqual({ include_usage: true });
      expect(request.body.temperature).toBe(0.3);
      expect(request.body.max_tokens).toBe(128);
      expect(JSON.stringify(request.body.messages)).toInclude("say hi");

      const exported = JSON.stringify(await service.exportSession("lane-cc"));
      expect(exported).toContain("Hello world");
      expect(exported).toInclude('"providerId":"loopback"');

      // Revocation: the next run must fail its provider request, not fire
      // with the retired key.
      expect(
        await NativeSessionServiceCtor.revokeProviderConnection(connectionId),
      ).toBe(true);
      expect(
        await NativeSessionServiceCtor.revokeProviderConnection(connectionId),
      ).toBe(false);
      const second = await service.send("lane-cc", { content: "again" });
      if (second.kind !== "turn") throw new Error("unreachable");
      const retry = await drainUntil(service, second.turnId);
      expect(retry.done).toBe(true);
      expect(retry.statuses.at(-1)).toBe("failed");
      expect(provider.requests).toHaveLength(1);
    } finally {
      await service?.close();
      await store.close();
      await provider.stop();
    }
  },
);

suite(
  "rust provider lane (responses): streams deltas and usage details",
  async () => {
    const provider = await startProviderMock();
    const store = await NativeStoreCtor.inMemory();
    let service: NativeSessionService | undefined;
    try {
      const connectionId =
        await NativeSessionServiceCtor.registerProviderConnection(
          `${provider.baseUrl}/`,
          "sk-loopback",
        );
      service = await store.createService({
        providerConfig: {
          protocol: "responses",
          providerId: "loopback-rs",
          connectionId,
          model: "model-r",
        },
      });
      await service.createSession({ sessionId: "lane-rs", profileId: "p1" });
      const sent = await service.send("lane-rs", { content: "say hi" });
      if (sent.kind !== "turn") throw new Error("unreachable");

      const { events, done, statuses } = await drainUntil(service, sent.turnId);
      expect(done).toBe(true);
      expect(statuses.at(-1)).toBe("completed");
      const deltas = events
        .map((u) => eventOf(u))
        .filter((e) => e?.type === "run.message.delta")
        .map((e) => e?.data?.delta);
      expect(deltas).toEqual(["Hel", "lo", " world"]);
      const reasoning = events
        .map((u) => eventOf(u))
        .find((e) => e?.type === "run.message.reasoning-delta");
      expect(reasoning?.data?.delta).toBe("thinking");
      const usage = events
        .map((u) => eventOf(u))
        .find((e) => e?.type === "run.usage");
      expect(usage?.data).toMatchObject({
        promptTokens: 7,
        completionTokens: 3,
        totalTokens: 10,
        cachedPromptTokens: 3,
        reasoningTokens: 1,
      });

      expect(provider.requests.length).toBeGreaterThanOrEqual(2);
      const first = provider.requests[0];
      const continuation = provider.requests[1];
      if (!first || !continuation)
        throw new Error("loopback captured fewer than two requests");
      expect(first.path).toBe("/responses");
      expect(first.headers.authorization).toBe("Bearer sk-loopback");
      expect(first.body.model).toBe("model-r");
      expect(first.body.stream).toBe(true);
      expect(first.body.store).toBe(false);
      expect(first.body.include).toEqual(["reasoning.encrypted_content"]);
      expect(JSON.stringify(first.body.input)).toInclude("say hi");
      // The reasoning item came back encrypted and the durable continuation
      // replayed it verbatim on the follow-up request.
      expect(JSON.stringify(continuation.body.input)).toInclude("enc_loopback");
      expect(
        (continuation.body.input as unknown[]).some(
          (item) =>
            item !== null &&
            typeof item === "object" &&
            "type" in item &&
            (item as { type: unknown }).type === "reasoning" &&
            "encrypted_content" in item &&
            (item as { encrypted_content: unknown }).encrypted_content ===
              "enc_loopback",
        ),
      ).toBe(true);
    } finally {
      await service?.close();
      await store.close();
      await provider.stop();
    }
  },
);

suite(
  "binding without a provider slot or lane settles the turn as failed",
  async () => {
    const store = await NativeStoreCtor.inMemory();
    let service: NativeSessionService | undefined;
    try {
      service = await store.createService();
      await service.createSession({ sessionId: "none", profileId: "p1" });
      const sent = await service.send("none", { content: "hi" });
      if (sent.kind !== "turn") throw new Error("unreachable");
      const { done, statuses } = await drainUntil(service, sent.turnId);
      expect(done).toBe(true);
      expect(statuses.at(-1)).toBe("failed");
    } finally {
      await service?.close();
      await store.close();
    }
  },
);

suite("js callback lane keeps single-chunk delta semantics", async () => {
  const store = await NativeStoreCtor.inMemory();
  let service: NativeSessionService | undefined;
  try {
    service = await store.createService();
    service.setProvider(() => ({
      content: "Hello world",
      finishReason: "stop",
    }));
    await service.createSession({
      sessionId: "js-lane",
      profileId: "p1",
      model: "test-model",
    });
    const sent = await service.send("js-lane", { content: "hi" });
    if (sent.kind !== "turn") throw new Error("unreachable");
    const { events, done, statuses } = await drainUntil(service, sent.turnId);
    expect(done).toBe(true);
    expect(statuses.at(-1)).toBe("completed");
    const deltas = events
      .map((u) => eventOf(u))
      .filter((e) => e?.type === "run.message.delta")
      .map((e) => e?.data?.delta);
    expect(deltas).toEqual(["Hello world"]);
  } finally {
    await service?.close();
    await store.close();
  }
});
