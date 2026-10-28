import { expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { available, missing, required } from "./helpers/binding.js";
import type {
  CreateSessionInputJson,
  NativeAgentError,
  NativeSessionService,
  NativeStore,
  ProviderRequest,
} from "../src/index.js";

// The classes are value bindings, assigned only when the artifacts exist.
let NativeAgentErrorCtor: typeof NativeAgentError;
let NativeStoreCtor: typeof NativeStore;
let bindingId: string;
let resolveCall: (
  callId: string,
  payload?: string | null,
  error?: string | null,
) => boolean;

if (required && !available) {
  throw new Error(
    `AGENTKIT_REQUIRE_NATIVE=1 but the binding is not built (missing ${missing.join(", ")}); run \`bun run build:native\` in packages/native`,
  );
}
if (available) {
  // Runtime-conditional import on purpose: the generated loader and the
  // platform `.node` binary do not exist before `bun run build:native`, so
  // a static import would break the whole file on ordinary TypeScript CI.
  const binding = await import("../src/index.js");
  const generated = await import("../generated/index.js");
  NativeAgentErrorCtor = binding.NativeAgentError;
  NativeStoreCtor = binding.NativeStore;
  bindingId = binding.bindingId;
  resolveCall = generated.resolveCall;
}

// When the build artifacts are absent (ordinary TypeScript CI), the whole
// suite reports as skipped; the required qualification run never reaches a
// skip without a binary — the throw above fails it closed instead.
const suite = available ? test : test.skip;

function disposableDir(): string {
  return mkdtempSync(join(tmpdir(), "agentkit-native-"));
}

function codeOf(error: unknown): string | undefined {
  return error instanceof NativeAgentErrorCtor
    ? (error as NativeAgentError).code
    : undefined;
}

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

function eventSeq(update: unknown): number | undefined {
  if (update && typeof update === "object" && "event" in update) {
    const event = update.event;
    if (event && typeof event === "object" && "seq" in event) {
      const seq = event.seq;
      if (typeof seq === "number") return seq;
    }
  }
  return undefined;
}

function eventKind(update: unknown): string | undefined {
  if (update && typeof update === "object" && "event" in update) {
    const event = update.event;
    if (
      event &&
      typeof event === "object" &&
      "event" in event &&
      event.event &&
      typeof event.event === "object" &&
      "type" in event.event
    ) {
      const kind = event.event.type;
      if (typeof kind === "string") return kind;
    }
  }
  return undefined;
}

async function drainUntil(
  service: NativeSessionService,
  taskId: string,
  timeoutMs = 20_000,
): Promise<{ events: unknown[]; done: boolean }> {
  const subscription = await service.subscribeTurn(taskId, 0);
  const events: unknown[] = [];
  const started = Date.now();
  let done = false;
  while (Date.now() - started < timeoutMs) {
    const batch = await subscription.poll(64, 100);
    events.push(...batch.events);
    done = batch.done;
    if (done) break;
  }
  subscription.cancel();
  return { events, done };
}

suite("binding identity probe reports the native addon", () => {
  expect(bindingId).toBe("agentkit-native");
});

suite("settling an unknown callback id is ignored, not an exception", () => {
  expect(resolveCall("no-such-call", "{}", null)).toBe(false);
  expect(resolveCall("no-such-call", null, "late")).toBe(false);
});

suite(
  "in-memory store runs a full chat turn through JS provider and JS tool",
  async () => {
    const store = await NativeStoreCtor.inMemory();
    const service = await store.createService();
    try {
      const providerCalls: ProviderRequest[] = [];
      service.setProvider((request) => {
        providerCalls.push(request);
        const userText = JSON.stringify(request.messages);
        if (providerCalls.length === 1) {
          // First request: ask for the JS-executed echo tool.
          return {
            content: "let me echo",
            toolCalls: [
              { id: "call-1", name: "echo", arguments: { text: "hello rust" } },
            ],
            finishReason: "tool_calls",
          };
        }
        if (!userText.includes("hello rust")) {
          throw new Error(
            `tool result never reached the provider: ${userText}`,
          );
        }
        return { content: "echoed: hello rust", finishReason: "stop" };
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
        (input) => {
          toolRuns += 1;
          const text =
            input &&
            typeof input === "object" &&
            "text" in input &&
            typeof input.text === "string"
              ? input.text
              : "";
          return { data: { echo: text } };
        },
      );

      await service.createSession({
        sessionId: "s1",
        profileId: "p1",
        model: "test-model",
      });
      const sent = await service.send("s1", { content: "please echo" });
      expect(sent.kind).toBe("turn");
      if (sent.kind !== "turn") throw new Error("unreachable");

      const { events, done } = await drainUntil(service, sent.turnId);
      expect(done).toBe(true);
      expect(events.length).toBeGreaterThan(0);

      // Sequence must be monotonic in seq and end with the settled update.
      let lastSeq = -1;
      let sawSettled = false;
      let sawToolEvent = false;
      for (const update of events) {
        if (updateType(update) === "settled") {
          sawSettled = true;
          expect(settledStatus(update)).toBe("completed");
          continue;
        }
        expect(updateType(update)).toBe("event");
        const seq = eventSeq(update) ?? -1;
        expect(seq).toBeGreaterThan(lastSeq);
        lastSeq = seq;
        if (eventKind(update) === "run.tool.succeeded") sawToolEvent = true;
      }
      expect(sawSettled).toBe(true);

      expect(toolRuns).toBe(1);
      expect(sawToolEvent).toBe(true);
      expect(providerCalls.length).toBeGreaterThanOrEqual(2);

      const turns = await service.listTurns("s1");
      expect(turns.length).toBeGreaterThanOrEqual(1);
      const exported = JSON.stringify(await service.exportSession("s1"));
      expect(exported).toContain("echoed: hello rust");
    } finally {
      await service.close();
      await store.close();
    }
  },
);

suite(
  "durable file store rejects a second owner and survives reopen",
  async () => {
    const dir = disposableDir();
    try {
      const path = join(dir, "agentkit.sqlite");
      const first = await NativeStoreCtor.open(path);
      let secondError: unknown;
      try {
        await NativeStoreCtor.open(path);
      } catch (error) {
        secondError = error;
      }
      expect(secondError).toBeInstanceOf(NativeAgentErrorCtor);
      expect(codeOf(secondError)).toBe("already_owned");
      // Create a session, then close everything and reopen from disk.
      const service = await first.createService();
      await service.createSession({
        sessionId: "s-persist",
        profileId: "p1",
        model: "test-model",
      });
      await service.close();
      await first.close();
      // Ownership released: reopening works and the session is still there.
      const reopened = await NativeStoreCtor.open(path);
      const reopenedService = await reopened.createService();
      const state = await reopenedService.getState("s-persist");
      const sessionId =
        "sessionId" in state && typeof state.sessionId === "string"
          ? state.sessionId
          : "";
      expect(sessionId).toBe("s-persist");
      await reopenedService.close();
      await reopened.close();
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  },
);

suite(
  "session failures cross the boundary as contract-coded errors",
  async () => {
    const store = await NativeStoreCtor.inMemory();
    const service = await store.createService();
    try {
      let error: unknown;
      try {
        await service.send("missing-session", { content: "hi" });
      } catch (caught) {
        error = caught;
      }
      expect(error).toBeInstanceOf(NativeAgentErrorCtor);
      expect(codeOf(error)).toBe("session_not_found");

      let inputError: unknown;
      try {
        await service.createSession({
          profileId: "",
        } as CreateSessionInputJson);
      } catch (caught) {
        inputError = caught;
      }
      expect(inputError).toBeInstanceOf(NativeAgentErrorCtor);
      expect(codeOf(inputError)).toBe("invalid_input");
    } finally {
      await service.close();
      await store.close();
    }
  },
);

suite("close is idempotent and later calls fail closed", async () => {
  const store = await NativeStoreCtor.inMemory();
  const service = await store.createService();
  await service.close();
  await service.close();
  let error: unknown;
  try {
    await service.getState("s1");
  } catch (caught) {
    error = caught;
  }
  expect(error).toBeInstanceOf(NativeAgentErrorCtor);
  expect(codeOf(error)).toBe("closed");
  await store.close();
  await store.close();
});

suite(
  "a provider that rejects fails the turn instead of faking success",
  async () => {
    const store = await NativeStoreCtor.inMemory();
    const service = await store.createService();
    try {
      service.setProvider(() => {
        throw new Error("provider exploded");
      });
      await service.createSession({
        sessionId: "s1",
        profileId: "p1",
        model: "test-model",
      });
      const sent = await service.send("s1", { content: "hi" });
      if (sent.kind !== "turn") throw new Error("expected a turn");
      const { events, done } = await drainUntil(service, sent.turnId);
      expect(done).toBe(true);
      const settled = events.find((update) => updateType(update) === "settled");
      expect(settledStatus(settled)).toBe("failed");
    } finally {
      await service.close();
      await store.close();
    }
  },
);
