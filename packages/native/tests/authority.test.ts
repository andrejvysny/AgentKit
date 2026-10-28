import { expect, test } from "bun:test";
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { available, missing, required } from "./helpers/binding.js";
import type {
  NativeAgentError,
  NativeSessionService,
  NativeStore,
  ProviderRequest,
  ProviderResult,
} from "../src/index.js";

// Authority and shutdown qualification for the public binding surface:
// plan mode must not let JS write tools act without approval, close must
// never dispatch a fresh callback, concurrent closes must share one
// completion, and the binding parser must not launder explicit nulls into
// absences the native descriptor reader rejects.

let NativeAgentErrorCtor: typeof NativeAgentError;
let NativeStoreCtor: typeof NativeStore;

if (required && !available) {
  throw new Error(
    `AGENTKIT_REQUIRE_NATIVE=1 but the binding is not built (missing ${missing.join(", ")}); run \`bun run build:native\` in packages/native`,
  );
}
if (available) {
  const binding = await import("../src/index.js");
  NativeAgentErrorCtor = binding.NativeAgentError;
  NativeStoreCtor = binding.NativeStore;
}

const suite = available ? test : test.skip;

function disposableDir(): string {
  return mkdtempSync(join(tmpdir(), "agentkit-authority-"));
}

function codeOf(error: unknown): string | undefined {
  return error instanceof NativeAgentErrorCtor
    ? (error as NativeAgentError).code
    : undefined;
}

function toolCall(id: string, name: string, args: unknown) {
  return {
    content: "",
    finishReason: "tool_calls",
    // The provider contract's field is `arguments` (the Rust decoder reads
    // entry["arguments"]); an object is serialized natively. Emitting a
    // lookalike key here would silently hand the tool `{}`.
    toolCalls: [{ id, name, arguments: args }],
  };
}

function final(text: string) {
  return { content: text, finishReason: "stop", toolCalls: [] };
}

/** Script a provider: one scripted result per model request. */
function scripted(...responses: unknown[]) {
  const calls: ProviderRequest[] = [];
  const provider = async (
    _request: ProviderRequest,
  ): Promise<ProviderResult> => {
    const index = calls.length;
    calls.push(_request);
    const response = responses[index];
    if (response === undefined) return final("script exhausted");
    return response as ProviderResult;
  };
  return { calls, provider };
}

async function waitFor<T>(
  read: () => Promise<T>,
  accept: (value: T) => boolean,
  timeoutMs = 10_000,
): Promise<T> {
  // Poll-and-backoff, not a duration wait: the loop exits the instant the
  // durable condition holds. Deterministic fake timers cannot control the
  // native runtime's own worker thread, so bounded polling is the only
  // observable-signal wait available here.
  const started = Date.now();
  let value = await read();
  while (!accept(value) && Date.now() - started < timeoutMs) {
    await Bun.sleep(20);
    value = await read();
  }
  return value;
}

function waitingRequestId(state: Record<string, unknown>): string | null {
  const waiting = state.waiting;
  if (waiting && typeof waiting === "object") {
    const record = waiting as Record<string, unknown>;
    const id = record.inputId;
    if (typeof id === "string") return id;
  }
  return null;
}

/** Canonical JSON — object keys sorted recursively, compact separators —
 * byte-for-byte what the Rust `args_sha256` hashes (BTreeMap-sorted tree →
 * serde compact). `arguments` sent through the helper must therefore be
 * visible in the durable ask's `argsSha256`: proof the human approved the
 * EXACT arguments the model sent, not `{}`. */
function canonicalJson(value: unknown): string {
  if (value === null || typeof value !== "object") return JSON.stringify(value);
  if (Array.isArray(value)) return `[${value.map(canonicalJson).join(",")}]`;
  const entries = Object.entries(value as Record<string, unknown>)
    .filter(([, v]) => v !== undefined)
    .sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0));
  return `{${entries.map(([k, v]) => `${JSON.stringify(k)}:${canonicalJson(v)}`).join(",")}}`;
}

function argsSha256(args: unknown): string {
  return new Bun.CryptoHasher("sha256")
    .update(canonicalJson(args))
    .digest("hex");
}

async function settle(
  service: NativeSessionService,
  taskId: string,
  timeoutMs = 20_000,
): Promise<string | undefined> {
  const subscription = await service.subscribeTurn(taskId, 0);
  const started = Date.now();
  let status: string | undefined;
  while (Date.now() - started < timeoutMs) {
    const batch = await subscription.poll(64, 100);
    for (const update of batch.events) {
      if (
        update &&
        typeof update === "object" &&
        "task" in update &&
        (update as Record<string, unknown>).task
      ) {
        const task = (update as Record<string, unknown>).task as Record<
          string,
          unknown
        >;
        if (typeof task.status === "string") status = task.status;
      }
    }
    if (batch.done) break;
  }
  subscription.cancel();
  return status;
}

suite(
  "plan mode never runs a JS write callback without an approval",
  async () => {
    const store = await NativeStoreCtor.inMemory();
    const service = await store.createService();
    const { provider } = scripted(
      toolCall("call-write", "write_back", { path: "a.txt" }),
      final("done"),
    );
    service.setProvider(provider);
    let writes = 0;
    await service.registerTool(
      {
        name: "write_back",
        effect: "write",
        capability: "fs.write",
        inputSchema: { type: "object" },
      },
      async () => {
        writes += 1;
        return { data: "written" };
      },
    );
    const state = await service.createSession({
      profileId: "authority",
      model: "test-model",
      executionMode: "plan",
    });
    const sessionId = state.sessionId as string;
    const sent = await service.send(sessionId, {
      content: { type: "text", text: "write it" },
    });
    if (sent.kind !== "turn") throw new Error("expected a turn");
    const status = await settle(service, sent.turnId);
    expect(writes).toBe(0);
    expect(waitingRequestId(await service.getState(sessionId))).toBeNull();
    expect((await service.getExecution(sessionId))?.mode).toBe("plan");
    expect(status).toBe("completed");
    await service.close();
    await store.close();
  },
);

suite(
  "plan approval escalates the session and the write acts exactly once",
  async () => {
    const dir = disposableDir();
    const path = join(dir, "plan-grant.db");
    const store = await NativeStoreCtor.open(path);
    const service = await store.createService({
      jsToolAuthority: { planToolNames: ["submit_plan"] },
    });
    let writes = 0;
    const { provider } = scripted(
      toolCall("call-plan", "submit_plan", {
        plan: "write a.txt",
        mode: "ask",
      }),
      toolCall("call-write", "write_back", { path: "a.txt" }),
      final("done"),
    );
    service.setProvider(provider);
    let writeInput: unknown;
    await service.registerTool(
      {
        name: "submit_plan",
        // Same metadata the NATIVE submit_plan declares: capability-driven
        // Read class — the name (not the effect) is what must route it to
        // the human, so a read-effect declaration cannot smuggle a plan
        // past the ask.
        effect: "read",
        capability: "plan",
        inputSchema: { type: "object" },
      },
      async () => {
        throw new Error("the plan tool body must never run");
      },
    );
    await service.registerTool(
      {
        name: "write_back",
        effect: "write",
        capability: "fs.write",
        inputSchema: { type: "object" },
      },
      async (input: unknown) => {
        writeInput = input;
        writes += 1;
        return { data: "written" };
      },
    );
    const created = await service.createSession({
      profileId: "authority",
      model: "test-model",
      executionMode: "plan",
    });
    const sessionId = created.sessionId as string;
    const sent = await service.send(sessionId, {
      content: { type: "text", text: "plan then write" },
    });

    // The plan proposal waits for the human; no write has acted.
    const planWait = await waitFor(
      () => service.getState(sessionId),
      (state) => waitingRequestId(state) !== null,
    );
    const planRequest = waitingRequestId(planWait);
    expect(planRequest).not.toBeNull();
    expect(writes).toBe(0);
    // The ask carries the REAL plan text and requested mode — the helper
    // passes `arguments`, so the human judges what the model actually
    // sent, not `{}`.
    const planAsk = planWait.waiting as Record<string, unknown> | undefined;
    const planContent = (planAsk?.content ?? {}) as Record<string, unknown>;
    expect(planContent.toolName).toBe("submit_plan");
    expect(planContent.prompt).toBe("write a.txt\n\n(requested mode: ask)");
    expect(planContent.requestedMode).toBe("ask");
    expect(planContent.argsSha256).toBe(
      argsSha256({ plan: "write a.txt", mode: "ask" }),
    );
    await service.approve(planRequest as string, "ans-plan", "go");
    expect((await service.getExecution(sessionId))?.mode).toBe("ask");

    // Ask mode: the write itself needs one per-call approval first.
    const writeWait = await waitFor(
      () => service.getState(sessionId),
      (state) => waitingRequestId(state) !== null,
    );
    const writeRequest = waitingRequestId(writeWait);
    expect(writeRequest).not.toBeNull();
    expect(writes).toBe(0);
    const writeAsk = writeWait.waiting as Record<string, unknown> | undefined;
    const writeContent = (writeAsk?.content ?? {}) as Record<string, unknown>;
    expect(writeContent.toolName).toBe("write_back");
    expect(writeContent.kind).toBe("approval");
    // The approved argument identity is EXACTLY what the model requested.
    expect(writeContent.argsSha256).toBe(argsSha256({ path: "a.txt" }));
    await service.approve(writeRequest as string, "ans-write");
    if (sent.kind !== "turn") throw new Error("expected a turn");
    const status = await settle(service, sent.turnId);
    expect(writes).toBe(1);
    // The callback received the EXACT arguments object the model requested.
    expect(writeInput).toEqual({ path: "a.txt" });
    expect(status).toBe("completed");

    // Reopen: replay must not re-dispatch the tool or duplicate its effect.
    await service.close();
    await store.close();
    const reopened = await NativeStoreCtor.open(path);
    const second = await reopened.createService();
    const replay = await second.getState(sessionId);
    expect(replay.sessionId).toBe(sessionId);
    expect(writes).toBe(1);
    await second.close();
    await reopened.close();
  },
);

suite(
  "auto_edit asks before a JS command tool runs, approval acts once",
  async () => {
    // The assessment's P1 through the PUBLIC binding under Node/Bun: a
    // JS tool with effect "write" but capability "process.exec" used to
    // run unapproved in auto_edit because the binding guard branched on
    // effect. The shared matrix classifies by capability: commands ask.
    const store = await NativeStoreCtor.inMemory();
    const service = await store.createService();
    const { provider } = scripted(
      toolCall("call-cmd", "run_cmd", { command: "ls -la" }),
      final("done"),
    );
    service.setProvider(provider);
    let runs = 0;
    let runInput: unknown;
    await service.registerTool(
      {
        name: "run_cmd",
        effect: "write",
        capability: "process.exec",
        inputSchema: { type: "object" },
      },
      async (input: unknown) => {
        runs += 1;
        runInput = input;
        return { data: "ok" };
      },
    );
    const created = await service.createSession({
      profileId: "authority",
      model: "test-model",
      executionMode: "auto_edit",
    });
    const sessionId = created.sessionId as string;
    const sent = await service.send(sessionId, {
      content: { type: "text", text: "list the files" },
    });
    if (sent.kind !== "turn") throw new Error("expected a turn");

    const waiting = await waitFor(
      () => service.getState(sessionId),
      (state) => waitingRequestId(state) !== null,
    );
    const request = waitingRequestId(waiting);
    expect(request).not.toBeNull();
    expect(runs).toBe(0);
    const ask = waiting.waiting as Record<string, unknown> | undefined;
    const content = (ask?.content ?? {}) as Record<string, unknown>;
    expect(content.kind).toBe("approval");
    expect(content.toolName).toBe("run_cmd");
    expect(content.prompt).toBe("Approve run_cmd?");
    // Approved argument identity, not the empty object the old
    // `argumentsJson` helper would have produced.
    expect(content.argsSha256).toBe(argsSha256({ command: "ls -la" }));
    const approval = waiting.waitingApproval as Record<string, unknown>;
    expect(approval.requestId).toBe(request);

    await service.approve(request as string, "ans-cmd");
    const status = await settle(service, sent.turnId);
    expect(status).toBe("completed");
    expect(runs).toBe(1);
    expect(runInput).toEqual({ command: "ls -la" });
    await service.close();
    await store.close();
  },
);

suite(
  "a read-effect plan submission still reaches the human in plan mode",
  async () => {
    // The assessment's related P2: the guard returned Allowed for ALL read
    // effects before the plan-tool check, so a JS submit_plan declaring
    // effect "read" ran without ever creating an approval request.
    const store = await NativeStoreCtor.inMemory();
    const service = await store.createService({
      jsToolAuthority: {
        planToolNames: ["submit_plan"],
        planGrantMode: "auto_edit",
      },
    });
    let planBodies = 0;
    const { provider } = scripted(
      toolCall("call-plan", "submit_plan", {
        plan: "touch a",
        mode: "auto_edit",
      }),
      final("done"),
    );
    service.setProvider(provider);
    await service.registerTool(
      {
        name: "submit_plan",
        effect: "read",
        capability: "plan",
        inputSchema: { type: "object" },
      },
      async () => {
        planBodies += 1;
        return { data: "plan recorded" };
      },
    );
    const created = await service.createSession({
      profileId: "authority",
      model: "test-model",
      executionMode: "plan",
    });
    const sessionId = created.sessionId as string;
    const sent = await service.send(sessionId, {
      content: { type: "text", text: "plan it" },
    });
    if (sent.kind !== "turn") throw new Error("expected a turn");

    const waiting = await waitFor(
      () => service.getState(sessionId),
      (state) => waitingRequestId(state) !== null,
    );
    // Nothing ran while the ask was unanswered — the read-effect
    // declaration no longer short-circuits the plan-tool check.
    expect(waitingRequestId(waiting)).not.toBeNull();
    expect(planBodies).toBe(0);
    const ask = waiting.waiting as Record<string, unknown> | undefined;
    const content = (ask?.content ?? {}) as Record<string, unknown>;
    expect(content.toolName).toBe("submit_plan");
    expect(content.prompt).toBe("touch a\n\n(requested mode: auto_edit)");
    expect(content.requestedMode).toBe("auto_edit");
    expect(content.argsSha256).toBe(
      argsSha256({ plan: "touch a", mode: "auto_edit" }),
    );
    const plan = waiting.waitingPlan as Record<string, unknown>;
    expect(String(plan.plan)).toContain("touch a");
    expect(plan.requestedMode).toBe("auto_edit");

    await service.approve(
      waitingRequestId(waiting) as string,
      "ans-plan",
      "go",
    );
    expect((await service.getExecution(sessionId))?.mode).toBe("auto_edit");
    const status = await settle(service, sent.turnId);
    expect(status).toBe("completed");
    // The body ran ONLY as the resumed, human-approved call — never while
    // the session was still in plan mode.
    expect(planBodies).toBe(1);
    await service.close();
    await store.close();
  },
);

suite("close dispatches no new callbacks and always completes", async () => {
  const store = await NativeStoreCtor.inMemory();
  const service = await store.createService({ closeTimeoutMs: 2_000 });
  let providerCalls = 0;
  // Deliberately never settles — the executor form is required because
  // this promise is the in-flight callback under test, never resolved.
  const first = new Promise<never>(() => {});
  service.setProvider(async () => {
    providerCalls += 1;
    if (providerCalls === 1) {
      return toolCall("call-hang", "hang", {}) as never;
    }
    // A dispatch during close would land here; the test forbids it.
    return first;
  });
  await service.registerTool(
    {
      name: "hang",
      effect: "read",
      capability: "read",
      inputSchema: { type: "object" },
    },
    () => first,
  );
  const created = await service.createSession({
    profileId: "authority",
    model: "test-model",
  });
  const sessionId = created.sessionId as string;
  await service.send(sessionId, {
    content: { type: "text", text: "hang please" },
  });
  // Wait until the tool callback is genuinely in flight.
  await waitFor(
    async () => providerCalls,
    (count) => count >= 1,
  );
  const at = providerCalls;
  const closing = service.close();
  // Watchdog, not a duration wait: the defect under test is close()
  // hanging forever, which only a real-time bound can observe.
  const { promise: watchdog, resolve: arm } =
    Promise.withResolvers<"timeout">();
  const timer = setTimeout(() => arm("timeout"), 6_000);
  const raced = await Promise.race([
    closing.then(() => "closed" as const),
    watchdog,
  ]);
  clearTimeout(timer);
  expect(raced).toBe("closed");
  // close() awaited worker drain, so at resolution every run has settled:
  // no post-close dispatch window exists to sleep through.
  expect(providerCalls).toBe(at);
  expect(service.closeUncertain).toBe(false);
  await store.close();
});

suite("concurrent close callers share one completion", async () => {
  const store = await NativeStoreCtor.inMemory();
  const service = await store.createService();
  const created = await service.createSession({
    profileId: "authority",
    model: "test-model",
  });
  expect(created.sessionId).toBeDefined();
  const first = service.close();
  const second = service.close();
  await Promise.all([first, second]);
  await store.close();
});

suite(
  "close leaves an in-flight JS callback observable, replay fabricates nothing",
  async () => {
    // The assessment's shutdown probe through the public surface: a write
    // callback is paused mid-effect; closing service+store must (a) not
    // claim its external effect stopped, (b) make it OBSERVABLE, and (c)
    // never re-dispatch or fabricate it on reopen.
    const dir = disposableDir();
    const path = join(dir, "paused-callback.db");
    const store = await NativeStoreCtor.open(path);
    const service = await store.createService({ closeTimeoutMs: 3_000 });
    let effects = 0;
    let dispatches = 0;
    let release!: () => void;
    const gate = new Promise<void>((resolve) => {
      release = resolve;
    });
    let providerCalls = 0;
    service.setProvider(async () => {
      providerCalls += 1;
      if (providerCalls === 1) {
        return toolCall("call-pause", "pause_me", { v: 1 }) as never;
      }
      // Must never run: the turn's only tool call is paused, not completed.
      return final("provider must not resume after close");
    });
    await service.registerTool(
      {
        name: "pause_me",
        effect: "write",
        capability: "fs.write",
        inputSchema: { type: "object" },
      },
      async () => {
        dispatches += 1;
        effects += 1;
        await gate;
        return { data: "late" };
      },
    );
    const created = await service.createSession({
      profileId: "authority",
      model: "test-model",
      executionMode: "full_auto",
    });
    const sessionId = created.sessionId as string;
    await service.send(sessionId, { content: { type: "text", text: "go" } });
    await waitFor(
      async () => effects,
      (count) => count >= 1,
    );
    expect(service.pendingCallbacks).toBe(1);

    await service.close();
    await store.close();
    // Worker drain was established — but the honest contract is that this
    // says nothing about the callback's EXTERNAL effect, which is still
    // running (its promise has not settled).
    expect(service.closeUncertain).toBe(false);
    expect(service.pendingCallbacks).toBe(1);

    // Fresh-process-equivalent reopen: no re-dispatch, no fabricated result.
    const reopened = await NativeStoreCtor.open(path);
    const second = await reopened.createService();
    const state = await second.getState(sessionId);
    expect(state.sessionId).toBe(sessionId);
    expect(effects).toBe(1);
    expect(dispatches).toBe(1);
    expect(providerCalls).toBe(1);

    // The late settlement is observable and settles the count honestly.
    release();
    await waitFor(
      async () => service.pendingCallbacks,
      (count) => count === 0,
    );
    expect(effects).toBe(1);
    await second.close();
    await reopened.close();
  },
);

suite(
  "the binding parser preserves malformed-present values natively reject",
  async () => {
    const store = await NativeStoreCtor.inMemory();
    const service = await store.createService();
    for (const key of ["workspace", "context", "skills"]) {
      const bad: Record<string, unknown> = {
        profileId: "authority",
        [key]: null,
      };
      let code: string | undefined;
      try {
        await service.createSession(bad as never);
      } catch (error) {
        code = codeOf(error);
      }
      expect(code).toBe("invalid_input");
    }
    for (const bad of [
      { profileId: "authority", workspace: "not-a-record" },
      { profileId: "authority", context: "final_answers" },
      { profileId: "authority", workspace: { root: "/p", trusted: null } },
      { profileId: "authority", skills: [{}] },
      { profileId: "authority", providerId: null },
      { profileId: "authority", model: null },
      { profileId: "authority", turnKind: null },
    ]) {
      let code: string | undefined;
      try {
        await service.createSession(bad as never);
      } catch (error) {
        code = codeOf(error);
      }
      expect(code).toBe("invalid_input");
    }
    const good = await service.createSession({
      profileId: "authority",
      workspace: { root: "/p", trusted: true },
      context: { policy: "transcript" },
    });
    expect(good.sessionId).toBeDefined();
    await service.close();
    await store.close();
  },
);
