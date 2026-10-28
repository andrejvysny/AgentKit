import { describe, expect, it } from "bun:test";
import type {
  AiProviderCapabilities,
  AiProviderKind,
  AiProviderModel,
  AiRunEvent,
} from "@agentkit/contracts";
import type {
  AiChatRequest,
  AiProviderClient,
  ModelRef,
  ModelRouter,
  ModelSelectionContext,
} from "@agentkit/core";
import {
  MockProviderClient,
  createTestEventStamper,
  nowIso,
} from "@agentkit/testing";
import {
  TurnRunner,
  type UsageAuthorizationDecision,
  type UsageAuthorizationRequest,
  type UsageAuthorizer,
  type UsageRecord,
} from "../src/index.js";
import { createHarness, type TestHarness } from "./fakes.js";

/**
 * Routing (Phase B/B3) through the host: who gets asked before a routed model
 * is dispatched, what a refusal costs, and what the usage meter bills a switch
 * against.
 *
 * The routing CONTRACT — a switch with no authorization is refused — is core's
 * and is tested there. What is tested here is the host half: that the host
 * actually puts the model the router CHOSE (not the one the turn was resolved
 * on) to its `UsageAuthorizer`, that a refusal never reaches a provider, that
 * the loop is handed the host's routing configuration at all, and that a routed
 * provider's own usage attribution survives to the recorder — which is what
 * makes a switch billable to the provider it moved to.
 */

/** What one scripted reply costs, in the tokens the meter must not lose. */
interface ScriptedUsage {
  callId: string;
  attempt: number;
  providerId?: string;
  promptTokens?: number;
  completionTokens?: number;
  totalTokens?: number;
  cachedPromptTokens?: number;
  reasoningTokens?: number;
}

interface ScriptedTurn {
  content: string;
  usage?: ScriptedUsage;
}

/**
 * A provider whose attribution fields the test chooses. {@link MockProviderClient}
 * emits a fixed usage report with no `providerId` and no cached/reasoning
 * numbers, and the fields these tests are about are exactly the ones a routed
 * provider adds — so this one reports what it is told to and records what it
 * was asked for.
 */
class ScriptedClient implements AiProviderClient {
  readonly kind: AiProviderKind = "openai-compatible";
  /** Provider round-trips this client served. */
  callCount = 0;
  /** The model each request asked for, in order. */
  readonly models: string[] = [];
  private turnIndex = 0;
  private readonly defaults = new MockProviderClient();

  constructor(
    readonly id: string,
    private readonly turns: readonly ScriptedTurn[],
  ) {}

  // The shared mock's own capability/model answers; the routing tests never
  // vary them, they only need a contract-valid client to stand behind.
  capabilities = (): Promise<AiProviderCapabilities> =>
    this.defaults.capabilities();

  listModels = (): Promise<AiProviderModel[]> => this.defaults.listModels();

  async *streamChat(input: AiChatRequest): AsyncIterable<AiRunEvent> {
    this.callCount++;
    this.models.push(input.model);
    const turn = this.turns[this.turnIndex] ?? { content: "" };
    this.turnIndex++;
    const stamp = createTestEventStamper();
    const runId = input.runId;
    yield stamp({
      type: "run.started",
      runId,
      timestamp: nowIso(),
      data: { model: input.model, toolCount: 0 },
    });
    yield stamp({
      type: "run.message.delta",
      runId,
      timestamp: nowIso(),
      data: { delta: turn.content },
    });
    yield stamp({
      type: "run.message.completed",
      runId,
      timestamp: nowIso(),
      data: { content: turn.content, toolCallCount: 0 },
    });
    const usage = turn.usage;
    if (usage === undefined) return;
    yield stamp({
      type: "run.usage",
      runId,
      timestamp: nowIso(),
      data: {
        callId: usage.callId,
        attempt: usage.attempt,
        step: 0,
        model: input.model,
        source: "response",
        finalForCall: true,
        ...(usage.providerId === undefined
          ? {}
          : { providerId: usage.providerId }),
        ...(usage.promptTokens === undefined
          ? {}
          : { promptTokens: usage.promptTokens }),
        ...(usage.completionTokens === undefined
          ? {}
          : { completionTokens: usage.completionTokens }),
        ...(usage.totalTokens === undefined
          ? {}
          : { totalTokens: usage.totalTokens }),
        ...(usage.cachedPromptTokens === undefined
          ? {}
          : { cachedPromptTokens: usage.cachedPromptTokens }),
        ...(usage.reasoningTokens === undefined
          ? {}
          : { reasoningTokens: usage.reasoningTokens }),
      },
    });
  }
}

/**
 * A `UsageAuthorizer` answering from a per-model script. The pass gate is
 * asked about the model the turn was RESOLVED on and the routing hook about the
 * model the ROUTER chose; keeping those distinguishable is the whole point,
 * since every assertion below depends on which pair came in.
 */
class ScriptedUsageAuthorizer implements UsageAuthorizer {
  readonly authorizeCalls: UsageAuthorizationRequest[] = [];
  readonly records: UsageRecord[] = [];

  constructor(
    private readonly answers: ReadonlyMap<
      string,
      UsageAuthorizationDecision
    > = new Map(),
    private readonly fallback: UsageAuthorizationDecision = { allowed: true },
  ) {}

  async authorize(
    request: UsageAuthorizationRequest,
  ): Promise<UsageAuthorizationDecision> {
    this.authorizeCalls.push(request);
    return (
      this.answers.get(`${request.providerId}/${request.model}`) ??
      this.fallback
    );
  }

  async record(usage: UsageRecord): Promise<void> {
    this.records.push(usage);
  }
}

/** A router that always answers the same thing, and remembers being asked. */
class FixedRouter implements ModelRouter {
  readonly contexts: ModelSelectionContext[] = [];
  constructor(private readonly answer: ModelRef) {}
  async selectModel(context: ModelSelectionContext): Promise<ModelRef> {
    this.contexts.push(context);
    return this.answer;
  }
}

const BASE = { id: "p1", model: "m1" } as const;
const ROUTED = { providerId: "p2", modelId: "m-cheap" } as const;

interface Fixture extends TestHarness {
  runner: TurnRunner;
  chatId: string;
  /** The provider the turn was resolved on — the one a switch leaves behind. */
  base: ScriptedClient;
  /** The client `resolveClient` hands out for a routed {@link ROUTED}. */
  routed: ScriptedClient;
  /** Which provider ids `resolveClient` was asked to resolve. */
  resolveCalls: string[];
}

interface SetupOptions {
  usage?: UsageAuthorizer;
  /** Absent: routing is not wired at all — the pre-B3 path. */
  router?: ModelRouter;
  requiredCapabilities?: readonly string[];
  continuationModelSwitch?: "reject" | "reset";
  baseTurns?: readonly ScriptedTurn[];
  routedTurns?: readonly ScriptedTurn[];
}

async function setup(options: SetupOptions): Promise<Fixture> {
  const harness = createHarness();
  const base = new ScriptedClient(
    BASE.id,
    options.baseTurns ?? [{ content: "from p1" }],
  );
  const routed = new ScriptedClient(
    ROUTED.providerId,
    options.routedTurns ?? [{ content: "from p2" }],
  );
  const resolveCalls: string[] = [];

  await harness.store.providers.upsertProvider({
    id: BASE.id,
    label: "Base",
    kind: "openai-compatible",
    baseUrl: "http://localhost:1234",
    defaultModel: BASE.model,
    enabled: true,
  });
  await harness.store.settings.updateSettings({ defaultProviderId: BASE.id });
  const chat = await harness.store.conversations.createChat({ id: "chat-1" });

  const runner = new TurnRunner({
    store: harness.store,
    taskRunner: harness.taskRunner,
    providerFactory: (config) => (config.id === BASE.id ? base : routed),
    contributors: [],
    clock: harness.clock,
    ids: harness.ids,
    ...(options.usage === undefined ? {} : { usage: options.usage }),
    ...(options.router === undefined
      ? {}
      : {
          routing: {
            router: options.router,
            resolveClient: (providerId: string) => {
              resolveCalls.push(providerId);
              return providerId === ROUTED.providerId
                ? routed
                : new ScriptedClient(providerId, []);
            },
            ...(options.requiredCapabilities === undefined
              ? {}
              : { requiredCapabilities: options.requiredCapabilities }),
            ...(options.continuationModelSwitch === undefined
              ? {}
              : { continuationModelSwitch: options.continuationModelSwitch }),
          },
        }),
  });
  return { ...harness, runner, chatId: chat.id, base, routed, resolveCalls };
}

/** Stand in for the task runner: create the attempt, take the lease, execute. */
async function drive(f: Fixture, taskId: string): Promise<void> {
  const attempt = await f.store.tasks.createAttempt({
    attemptId: f.ids.attemptId(),
    taskId,
    ownerId: "worker-1",
  });
  const lease = await f.store.tasks.acquireLease({
    taskId,
    attemptId: attempt.attemptId,
    ownerId: "worker-1",
    ttlMs: 60_000,
  });
  await f.runner.execute({
    taskId,
    attemptId: attempt.attemptId,
    leaseToken: lease.leaseToken,
    signal: new AbortController().signal,
  });
}

async function eventsOf(f: Fixture, taskId: string): Promise<AiRunEvent[]> {
  return (await f.store.tasks.listEvents(taskId)) as AiRunEvent[];
}

/** One submitted turn on the session's resolved provider/model (`p1/m1`). */
async function submitTurn(f: Fixture): Promise<string> {
  const submitted = await f.runner.submitMessage({
    chatId: f.chatId,
    content: "hello",
    model: BASE.model,
  });
  return submitted.runId;
}

describe("TurnRunner routing — the routed selection is authorized", () => {
  it("asks the usage authorizer about the model the router chose, and dispatches only on it", async () => {
    const usage = new ScriptedUsageAuthorizer();
    const f = await setup({ usage, router: new FixedRouter({ ...ROUTED }) });
    const runId = await submitTurn(f);
    await drive(f, runId);

    // Once for the pass gate (the resolved model), once for the switch — the
    // routed pair, the only authorization whose answer matters here.
    expect(
      usage.authorizeCalls.map((call) => `${call.providerId}/${call.model}`),
    ).toEqual([
      `${BASE.id}/${BASE.model}`,
      `${ROUTED.providerId}/${ROUTED.modelId}`,
    ]);
    const routedCall = usage.authorizeCalls.at(-1);
    expect(routedCall?.runId).toBe(runId);
    expect(routedCall?.chatId).toBe(f.chatId);
    expect(typeof routedCall?.estimatedPromptTokens).toBe("number");

    // Nothing ran on the provider the run started on; the answer came from the
    // routed one, asked for by id and for the routed model.
    expect(f.base.callCount).toBe(0);
    expect(f.routed.callCount).toBe(1);
    expect(f.routed.models).toEqual([ROUTED.modelId]);
    expect(f.resolveCalls).toEqual([ROUTED.providerId]);

    const events = await eventsOf(f, runId);
    expect(events.map((event) => event.type)).toContain("run.completed");
    const path = await f.store.conversations.listMessages(f.chatId);
    const visible = path.filter(
      (message) =>
        message.role === "assistant" && message.metadata["internal"] !== true,
    );
    expect(visible.at(-1)?.content).toBe("from p2");
  });

  it("asks before it resolves a client, so a refusal never needs one", async () => {
    const usage = new ScriptedUsageAuthorizer(
      new Map([
        [
          `${ROUTED.providerId}/${ROUTED.modelId}`,
          { allowed: false, reason: "not on the allow-list" },
        ],
      ]),
    );
    const f = await setup({ usage, router: new FixedRouter({ ...ROUTED }) });
    const runId = await submitTurn(f);
    await drive(f, runId);

    // No dispatch on the refused provider, and no client resolved in order to
    // dispatch with it: the ask comes before `resolveClient`, so a refusal
    // never reaches the point where a second provider's credential is needed.
    expect(f.resolveCalls).toEqual([]);
    expect(f.routed.callCount).toBe(0);
    expect(f.base.callCount).toBe(0);
  });

  it("ends the run with a routing denial on the durable log and fails the task", async () => {
    const usage = new ScriptedUsageAuthorizer(
      new Map([
        [
          `${ROUTED.providerId}/${ROUTED.modelId}`,
          { allowed: false, reason: "too expensive for this chat" },
        ],
      ]),
    );
    const f = await setup({ usage, router: new FixedRouter({ ...ROUTED }) });
    const runId = await submitTurn(f);
    await drive(f, runId);

    const events = await eventsOf(f, runId);
    const failed = events.find((event) => event.type === "run.failed");
    if (failed?.type !== "run.failed") throw new Error("expected run.failed");
    expect(failed.data.errorCode).toBe("authorization_denied");
    expect(failed.data.errorMessage).toContain("too expensive for this chat");
    // Nothing ran: the switch was refused before a byte was dispatched, so the
    // only things on the log are the terminal and the host's own "no answer"
    // warning that every content-less pass gets.
    expect(events.map((event) => event.type)).toEqual([
      "run.failed",
      "run.warning",
    ]);
    const warning = events[1];
    if (warning?.type !== "run.warning")
      throw new Error("expected run.warning");
    expect(warning.data.code).toBe("empty_response");
    expect((await f.store.tasks.getTask(runId))?.status).toBe("failed");
    // A call that never happened is billed to nobody.
    expect(usage.records).toEqual([]);
  });

  it("routes under the host's opt-in and consults no router without it", async () => {
    const usage = new ScriptedUsageAuthorizer();
    const f = await setup({ usage });
    const runId = await submitTurn(f);
    await drive(f, runId);

    // No `routing` dep: the turn dispatches on its own resolved provider/model
    // and the authorizer is asked exactly the one pass-gate question — the
    // routing hook adds no second gate to a turn that is not routed.
    expect(f.base.callCount).toBe(1);
    expect(f.routed.callCount).toBe(0);
    expect(usage.authorizeCalls.length).toBe(1);
  });

  it("hands the loop the host's capability requirement and continuation policy", async () => {
    const router = new FixedRouter({ ...ROUTED });
    const f = await setup({
      usage: new ScriptedUsageAuthorizer(),
      router,
      requiredCapabilities: ["tools", "vision"],
      continuationModelSwitch: "reset",
    });
    const runId = await submitTurn(f);
    await drive(f, runId);

    // The values only reach the router through the pass's wiring, so what the
    // router saw is the evidence that `deps.routing` was passed through.
    expect(router.contexts.length).toBeGreaterThan(0);
    expect(router.contexts[0]?.requiredCapabilities).toEqual([
      "tools",
      "vision",
    ]);
    expect(router.contexts[0]?.runId).toBe(runId);
    expect(router.contexts[0]?.step).toBe(1);
    expect(f.routed.callCount).toBe(1);
  });
});

describe("TurnRunner routing — the meter bills the provider that ran", () => {
  const routedSetup = (usage: UsageAuthorizer, routedTurns: ScriptedTurn[]) =>
    setup({
      usage,
      router: new FixedRouter({ ...ROUTED }),
      routedTurns,
    });

  it("records the event's providerId when it differs from the session's", async () => {
    const usage = new ScriptedUsageAuthorizer();
    const f = await routedSetup(usage, [
      {
        content: "from p2",
        usage: {
          callId: "p2-call-1",
          attempt: 1,
          // The routed client's own attribution. The session's provider is
          // still `p1`, and billing this call to `p1` is the bug.
          providerId: ROUTED.providerId,
          promptTokens: 40,
          completionTokens: 8,
          totalTokens: 48,
          cachedPromptTokens: 32,
          reasoningTokens: 5,
        },
      },
    ]);
    const runId = await submitTurn(f);
    await drive(f, runId);

    expect(usage.records.length).toBe(1);
    expect(usage.records[0]).toMatchObject({
      runId,
      providerId: ROUTED.providerId,
      model: ROUTED.modelId,
      promptTokens: 40,
      completionTokens: 8,
      totalTokens: 48,
      // The two numbers a cost estimate gets wrong in opposite directions if
      // the meter drops them: cached prompt tokens are discounted, reasoning
      // tokens are billed at the output rate.
      cachedPromptTokens: 32,
      reasoningTokens: 5,
      finalForCall: true,
      source: "response",
    });
  });

  it("passes the event's callId and attempt through verbatim", async () => {
    const usage = new ScriptedUsageAuthorizer();
    const f = await routedSetup(usage, [
      { content: "from p2", usage: { callId: "p2-call-42", attempt: 1 } },
    ]);
    const runId = await submitTurn(f);
    await drive(f, runId);

    const record = usage.records[0];
    // Identity, not counting: these are the keys a recorder groups interim
    // reports with the settled numbers of the same provider call.
    const event = (await eventsOf(f, runId)).find(
      (candidate) => candidate.type === "run.usage",
    );
    if (event?.type !== "run.usage") throw new Error("expected run.usage");
    expect(record?.callId).toBe("p2-call-42");
    expect(record?.attempt).toBe(event.data.attempt);
    expect(record?.callId).toBe(event.data.callId);
  });

  it("omits the token fields a provider never reported", async () => {
    const usage = new ScriptedUsageAuthorizer();
    const f = await routedSetup(usage, [
      { content: "from p2", usage: { callId: "p2-call-2", attempt: 1 } },
    ]);
    const runId = await submitTurn(f);
    await drive(f, runId);

    const record = usage.records[0];
    // Absent, not zero: a recorder prices a reported zero cached tokens as an
    // uncached call, and there is no honest difference between "free" and
    // "unknown" once the zero is written down.
    expect(record && "cachedPromptTokens" in record).toBe(false);
    expect(record && "reasoningTokens" in record).toBe(false);
    expect(record && "promptTokens" in record).toBe(false);
  });

  it("falls back to the session's provider when the event names none", async () => {
    const usage = new ScriptedUsageAuthorizer();
    const f = await routedSetup(usage, [
      { content: "from p2", usage: { callId: "anon-call", attempt: 1 } },
    ]);
    const runId = await submitTurn(f);
    await drive(f, runId);

    // Unattributed but never dropped: a budget that never hears about a call
    // cannot refuse the next one.
    expect(usage.records.map((record) => record.providerId)).toEqual([BASE.id]);
  });
});
