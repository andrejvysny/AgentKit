/**
 * Provider/config parity at the public binding: (1) a persisted model
 * catalogue decides context admission, (2) `toolCalling` settings and
 * recorded provider capabilities decide whether tools are staged — with
 * wire-capture as the witness, (3) an account-backed JS provider lane is
 * never stamped with an implicit output cap, and (4) revoking a provider
 * connection fails runs with the structured `connection_changed`, never a
 * misclassified `network_error`.
 */
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
  events: { type: string; data?: Record<string, unknown> }[];
  done: boolean;
  statuses: (string | undefined)[];
}> {
  // No cursor: afterSeq is EXCLUSIVE and the passing admission owns seq 0.
  const subscription = await service.subscribeTurn(taskId);
  const events: { type: string; data?: Record<string, unknown> }[] = [];
  const statuses: (string | undefined)[] = [];
  const started = Date.now();
  let done = false;
  while (Date.now() - started < timeoutMs) {
    const batch = await subscription.poll(64, 100);
    for (const update of batch.events) {
      if (updateType(update) === "settled")
        statuses.push(settledStatus(update));
      else {
        const event = eventOf(update);
        if (event) events.push(event);
      }
    }
    done = batch.done;
    if (done) break;
  }
  subscription.cancel();
  return { events, done, statuses };
}

suite(
  "stored model catalogue decides admission: a persisted 64-token window is admitted against 64, not the fallback",
  async () => {
    const provider = await startProviderMock();
    const store = await NativeStoreCtor.inMemory();
    let service: NativeSessionService | undefined;
    try {
      // The persisted catalogue via the public store port — the same door
      // the assistant store uses (`providers.replaceModels`).
      await store.callStore("providers.replaceModels", {
        providerId: "catalog",
        models: [
          {
            providerId: "catalog",
            modelId: "tiny-model",
            displayName: null,
            contextWindowTokens: 64,
            fetchedAt: "2026-01-01T00:00:00.000Z",
          },
          {
            providerId: "catalog",
            modelId: "unlisted-model",
            displayName: null,
            fetchedAt: "2026-01-01T00:00:00.000Z",
          },
        ],
      });
      const connectionId =
        await NativeSessionServiceCtor.registerProviderConnection(
          provider.baseUrl,
          "sk-loopback",
        );
      service = await store.createService({
        providerConfig: {
          protocol: "chat-completions",
          providerId: "catalog",
          connectionId,
          model: "tiny-model",
        },
      });

      // (a) No descriptor override: the STORED entry decides the window.
      await service.createSession({
        sessionId: "cat-seed",
        profileId: "p1",
        model: "tiny-model",
      });
      const sent = await service.send("cat-seed", { content: "say hi" });
      expect(sent.kind).toBe("turn");
      if (sent.kind !== "turn") throw new Error("unreachable");
      const seeded = await drainUntil(service, sent.turnId);
      expect(seeded.done).toBe(true);
      expect(seeded.statuses.at(-1)).toBe("completed");
      const admission = seeded.events.find(
        (event) => event.type === "run.context.admitted",
      );
      if (!admission?.data) throw new Error("no admission event");
      expect(admission.data.windowTokens).toBe(64);
      expect(admission.data.windowSource).toBe("model");
      // The wire carries the admitted cap, proving the window is REAL:
      // reserve is half the 64-token window and max_tokens equals it.
      expect(admission.data.reserveTokens).toBe(32);
      expect(admission.data.maxOutputTokens).toBe(32);
      const wire = provider.requests[0];
      if (!wire) throw new Error("loopback captured no request");
      expect(wire.body.max_tokens).toBe(32);

      // (b) Oversized against the 64-token window: refused at admission,
      // the provider is never called.
      await service.createSession({
        sessionId: "cat-oversized",
        profileId: "p1",
        model: "tiny-model",
      });
      const oversized = await service.send("cat-oversized", {
        content: "x".repeat(2000),
      });
      if (oversized.kind !== "turn") throw new Error("expected a turn");
      const overflow = await drainUntil(service, oversized.turnId);
      expect(overflow.statuses.at(-1)).toBe("failed");
      expect(
        overflow.events.some(
          (event) =>
            event.type === "run.failed" &&
            event.data?.errorCode === "context_overflow",
        ),
      ).toBe(true);
      expect(
        overflow.events.some((event) => event.type === "run.context.admitted"),
      ).toBe(false);
      expect(provider.requests).toHaveLength(1);

      // (c) The same 64 admitted through the DESCRIPTOR override still
      // wins over the stored entry.
      await service.createSession({
        sessionId: "cat-descriptor",
        profileId: "p1",
        model: "tiny-model",
        context: { windowTokens: 4096, reserveTokens: 16 },
      });
      const viaDescriptor = await service.send("cat-descriptor", {
        content: "say hi",
      });
      if (viaDescriptor.kind !== "turn") throw new Error("expected a turn");
      const overridden = await drainUntil(service, viaDescriptor.turnId);
      expect(overridden.statuses.at(-1)).toBe("completed");
      const descriptorAdmission = overridden.events.find(
        (event) => event.type === "run.context.admitted",
      );
      if (!descriptorAdmission?.data)
        throw new Error("no admission event (descriptor)");
      expect(descriptorAdmission.data.windowTokens).toBe(4096);
      expect(descriptorAdmission.data.windowSource).toBe("descriptor");

      // (d) Control — the pre-fix shape: a model NEITHER catalogue row
      // documents keeps the honest 32,768 fallback.
      await service.createSession({
        sessionId: "cat-fallback",
        profileId: "p1",
        model: "unlisted-model",
      });
      const unlisted = await service.send("cat-fallback", {
        content: "say hi",
      });
      if (unlisted.kind !== "turn") throw new Error("expected a turn");
      const fellBack = await drainUntil(service, unlisted.turnId);
      expect(fellBack.statuses.at(-1)).toBe("completed");
      const fallbackAdmission = fellBack.events.find(
        (event) => event.type === "run.context.admitted",
      );
      if (!fallbackAdmission?.data)
        throw new Error("no admission event (fallback)");
      expect(fallbackAdmission.data.windowTokens).toBe(32768);
      expect(fallbackAdmission.data.windowSource).toBe("fallback");
    } finally {
      await service?.close();
      await store.close();
      await provider.stop();
    }
  },
);

async function toolGateCase(
  settings: { toolCalling?: "auto" | "on" | "off" } | undefined,
  capabilities:
    | { streaming: boolean; toolCalling: boolean; modelList: boolean }
    | undefined,
): Promise<{
  bodies: Record<string, unknown>[];
  events: { type: string; data?: Record<string, unknown> }[];
  status: string | undefined;
  toolRuns: number;
}> {
  const provider = await startProviderMock({
    chatStages: [
      {
        deltas: [],
        toolCalls: [
          { id: "call-echo", name: "echo", arguments: { text: "sneaky" } },
        ],
      },
      { deltas: ["concluded"] },
    ],
  });
  const store = await NativeStoreCtor.inMemory();
  let service: NativeSessionService | undefined;
  try {
    if (capabilities !== undefined)
      await store.callStore("providers.saveCapabilities", {
        providerId: "gate",
        capabilities,
      });
    if (settings !== undefined)
      await store.callStore("settings.updateSettings", { patch: settings });
    const connectionId =
      await NativeSessionServiceCtor.registerProviderConnection(
        provider.baseUrl,
        "sk-loopback",
      );
    service = await store.createService({
      providerConfig: {
        protocol: "chat-completions",
        providerId: "gate",
        connectionId,
        model: "gate-model",
      },
    });
    let toolRuns = 0;
    await service.registerTool(
      {
        name: "echo",
        version: "1.0.0",
        effect: "read",
        capability: "echo",
        description: "Echo the text back",
        inputSchema: {
          type: "object",
          properties: { text: { type: "string" } },
          required: ["text"],
        },
      },
      () => {
        toolRuns += 1;
        return { data: { echo: "ran" } };
      },
    );
    await service.createSession({
      sessionId: "gate",
      profileId: "p1",
      model: "gate-model",
    });
    const sent = await service.send("gate", { content: "use the tool" });
    if (sent.kind !== "turn") throw new Error("expected a turn");
    const drained = await drainUntil(service, sent.turnId);
    return {
      bodies: provider.requests.map((request) => request.body),
      events: drained.events,
      status: drained.statuses.at(-1),
      toolRuns,
    };
  } finally {
    await service?.close();
    await store.close();
    await provider.stop();
  }
}

suite(
  "toolCalling off: no tools field on the wire, provider-emitted function_call refuses as tool_missing, counters prove no execution",
  async () => {
    const result = await toolGateCase({ toolCalling: "off" }, undefined);
    expect(result.bodies.length).toBeGreaterThanOrEqual(1);
    for (const body of result.bodies) {
      expect(body).not.toHaveProperty("tools");
      expect(body).not.toHaveProperty("tool_choice");
    }
    // The provider asked anyway; the frozen loop refuses without executing.
    expect(
      result.events.some(
        (event) =>
          event.type === "run.tool.failed" &&
          event.data?.errorCode === "tool_missing",
      ),
    ).toBe(true);
    expect(
      result.events.some((event) => event.type === "run.tool.succeeded"),
    ).toBe(false);
    expect(result.toolRuns).toBe(0);
    // The refusal is a RESULT fed back to the provider — the turn still
    // concludes (stage 2 answers), never a crash.
    expect(result.status).toBe("completed");
  },
);

suite(
  "auto with recorded toolCalling:false capability: same refusal — no tools on the wire, no execution",
  async () => {
    const result = await toolGateCase(
      undefined, // "auto" is the absent default
      { streaming: true, toolCalling: false, modelList: true },
    );
    expect(result.bodies.length).toBeGreaterThanOrEqual(1);
    for (const body of result.bodies) {
      expect(body).not.toHaveProperty("tools");
    }
    expect(
      result.events.some(
        (event) =>
          event.type === "run.tool.failed" &&
          event.data?.errorCode === "tool_missing",
      ),
    ).toBe(true);
    expect(result.toolRuns).toBe(0);
    expect(result.status).toBe("completed");
  },
);

suite(
  "on overrides a toolCalling:false probe: tools advertise and execute (the knob works both directions)",
  async () => {
    const result = await toolGateCase(
      { toolCalling: "on" },
      { streaming: true, toolCalling: false, modelList: true },
    );
    const tools = result.bodies[0]?.tools;
    if (!Array.isArray(tools)) throw new Error("no tools field on the wire");
    expect(JSON.stringify(tools)).toInclude("echo");
    expect(
      result.events.some((event) => event.type === "run.tool.succeeded"),
    ).toBe(true);
    expect(result.toolRuns).toBe(1);
    expect(result.status).toBe("completed");
  },
);

suite(
  "account-backed JS provider lane completes a turn: no implicit output cap is injected, the API-key lane is unchanged",
  async () => {
    const accountAdapter = (request: {
      maxOutputTokens?: number | null;
      temperature?: number | null;
    }): { content: string; finishReason: string } => {
      // An account-backed adapter declares NO sampling/output parameters —
      // the TS `responses-request.ts` account shape refuses them before
      // any HTTP (`unsupported_parameter`).
      if (
        typeof request.maxOutputTokens === "number" ||
        typeof request.temperature === "number"
      )
        throw new Error("unsupported_parameter");
      return { content: "account answer", finishReason: "stop" };
    };

    // Account lane: the turn COMPLETES — the 16,384 implicit cap must never
    // be injected, and undeclared options are omitted from the payload.
    const accountStore = await NativeStoreCtor.inMemory();
    let accountService: NativeSessionService | undefined;
    const seen: Record<string, unknown>[] = [];
    try {
      accountService = await accountStore.createService({
        providerAuthKind: "chatgpt-account",
      });
      accountService.setProvider((request) => {
        seen.push(request as unknown as Record<string, unknown>);
        return accountAdapter(request);
      });
      await accountService.createSession({
        sessionId: "acct",
        profileId: "p1",
        model: "acct-model",
      });
      const sent = await accountService.send("acct", { content: "hello" });
      if (sent.kind !== "turn") throw new Error("expected a turn");
      const drained = await drainUntil(accountService, sent.turnId);
      expect(drained.statuses.at(-1)).toBe("completed");
      expect(seen.length).toBe(1);
      expect("maxOutputTokens" in seen[0]!).toBe(false);
      expect("temperature" in seen[0]!).toBe(false);
      const admission = drained.events.find(
        (event) => event.type === "run.context.admitted",
      );
      if (!admission?.data) throw new Error("no admission event");
      expect(admission.data.windowSource).toBe("fallback");
      expect(admission.data).not.toHaveProperty("maxOutputTokens");
    } finally {
      await accountService?.close();
      await accountStore.close();
    }

    // API-key control: the default lane still stamps the implicit cap —
    // the same account-style adapter sees the number and refuses, proving
    // the lane itself is unchanged.
    const keyStore = await NativeStoreCtor.inMemory();
    let keyService: NativeSessionService | undefined;
    const keySeen: Record<string, unknown>[] = [];
    try {
      keyService = await keyStore.createService();
      keyService.setProvider((request) => {
        keySeen.push(request as unknown as Record<string, unknown>);
        return accountAdapter(request);
      });
      await keyService.createSession({
        sessionId: "key",
        profileId: "p1",
        model: "key-model",
      });
      const sent = await keyService.send("key", { content: "hello" });
      if (sent.kind !== "turn") throw new Error("expected a turn");
      const drained = await drainUntil(keyService, sent.turnId);
      expect(keySeen.length).toBe(1);
      expect(keySeen[0]!.maxOutputTokens).toBe(16384);
      expect(drained.statuses.at(-1)).toBe("failed");
      expect(
        drained.events.some(
          (event) =>
            event.type === "run.failed" &&
            event.data?.errorCode === "provider_callback_failed",
        ),
      ).toBe(true);
    } finally {
      await keyService?.close();
      await keyStore.close();
    }
  },
);

suite(
  "revoking a provider connection fails the next request with connection_changed, never network_error",
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
      service = await store.createService({
        providerConfig: {
          protocol: "chat-completions",
          providerId: "revoked",
          connectionId,
          model: "revoked-model",
        },
      });
      expect(
        await NativeSessionServiceCtor.revokeProviderConnection(connectionId),
      ).toBe(true);
      await service.createSession({
        sessionId: "revoked",
        profileId: "p1",
        model: "revoked-model",
      });
      const sent = await service.send("revoked", { content: "say hi" });
      if (sent.kind !== "turn") throw new Error("expected a turn");
      const drained = await drainUntil(service, sent.turnId);
      expect(drained.statuses.at(-1)).toBe("failed");
      const failure = drained.events.find(
        (event) => event.type === "run.failed",
      );
      if (!failure?.data) throw new Error("no run.failed event");
      expect(failure.data.errorCode).toBe("connection_changed");
      expect(
        drained.events.some(
          (event) =>
            event.type === "run.failed" &&
            event.data?.errorCode === "network_error",
        ),
      ).toBe(false);
      // No HTTP ever left for the revoked connection.
      expect(provider.requests).toHaveLength(0);
    } finally {
      await service?.close();
      await store.close();
      await provider.stop();
    }
  },
);
