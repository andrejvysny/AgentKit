/**
 * ONEMIND-349 · M0 fault matrix §A row 20 — MODEL CONTEXT OVERFLOW.
 *
 * `MACOS_ACCEPTANCE.md` §8 states the promise this suite makes executable:
 * "Test the estimated context threshold and one token over it; overflow must
 * be explicit, with durable history unchanged." Until now that edge existed
 * only as a manual acceptance step (and only the far-over case — a 40 000
 * character request — had an automated check in `context-transcript.test.ts`).
 *
 * A deterministic char-counting token estimator turns "the threshold" into a
 * computable number: the probe run captures the provider-facing message list,
 * the estimator prices it exactly, and the two graded runs sit at `budget`
 * and `budget + 1` tokens. Three assertions per the §8 sentence:
 *
 * - at the threshold the turn is ADMITTED: provider called once, task
 *   completes, no overflow error anywhere;
 * - one token over, the refusal is EXPLICIT before any provider call: a
 *   coded `context_overflow` failure with the estimated/budget/window numbers
 *   in the message, `run.failed` on the durable log, task landed `failed`;
 * - the DURABLE HISTORY is unchanged across the refusal (byte-identical
 *   message list, the rejected user message retained, no partial answer),
 *   nothing is left parked on the failed task, and capacity is released: the
 *   next turn submits and completes normally.
 */
import { describe, expect, it } from "bun:test";
import type { AiChatMessage, AiRunEvent } from "@agentkit/contracts";
import type { AiChatRequest } from "@agentkit/core";
import { MockProviderClient } from "@agentkit/testing";
import type { TokenEstimator } from "@agentkit/core";
import {
  SESSION_METADATA_KEY,
  TurnRunner,
  type TurnRunnerDeps,
} from "../src/index.js";
import {
  FakeAgentInputStore,
  createHarness,
  type TestHarness,
} from "./fakes.js";

/** One token per character: the threshold becomes an exact, checkable number. */
const estimator: TokenEstimator = {
  estimateMessage: (message) =>
    typeof message.content === "string"
      ? message.content.length
      : JSON.stringify(message.content ?? "").length,
  estimateText: (text) => text.length,
};

const RESERVE_TOKENS = 50;
const SYSTEM_PROMPT = "Stable system prompt for the overflow probe.";
const WARMUP = "warm-up question";
const PROBE = `probe question ${"p".repeat(300)}`;

class SpyProvider extends MockProviderClient {
  readonly requests: AiChatMessage[][] = [];
  override streamChat(input: AiChatRequest): AsyncIterable<AiRunEvent> {
    this.requests.push(structuredClone(input.messages));
    return super.streamChat(input);
  }
}

interface OverflowFixture {
  harness: TestHarness;
  mock: SpyProvider;
  chatId: string;
  inputs: FakeAgentInputStore;
  runner: TurnRunner;
}

async function setup(windowTokens: number): Promise<OverflowFixture> {
  const harness = createHarness();
  const mock = new SpyProvider();
  // One scripted text turn per provider call: a turn answers in exactly one
  // round-trip, so `callCount` is the provider-call count the suite grades.
  mock.setScript(
    Array.from({ length: 8 }, (_unused, index) => ({
      steps: [{ kind: "text" as const, content: `answer ${index + 1}` }],
    })),
  );
  await harness.store.providers.upsertProvider({
    id: "p1",
    label: "Mock",
    kind: "openai-compatible",
    baseUrl: "http://localhost:1234",
    defaultModel: "m1",
    enabled: true,
  });
  await harness.store.settings.updateSettings({ defaultProviderId: "p1" });
  const inputs = new FakeAgentInputStore(harness.clock);
  harness.store.inputs = inputs;
  const chat = await harness.store.conversations.createChat({
    id: "chat-1",
    metadata: {
      [SESSION_METADATA_KEY]: {
        version: 1,
        profile: { id: "default" },
        providerId: "p1",
        model: "m1",
        createdAt: "2026-01-01T00:00:00.000Z",
        context: { windowTokens, reserveTokens: RESERVE_TOKENS },
      },
    },
  });
  const deps: TurnRunnerDeps = {
    store: harness.store,
    taskRunner: harness.taskRunner,
    providerFactory: () => mock,
    // No tools: the tool-definition cost stays out of the arithmetic.
    contributors: [],
    context: {
      listBindings: async () => [],
      systemPrompt: async () => SYSTEM_PROMPT,
    },
    tokenEstimator: estimator,
    clock: harness.clock,
    ids: harness.ids,
  };
  return {
    harness,
    mock,
    chatId: chat.id,
    inputs,
    runner: new TurnRunner(deps),
  };
}

async function drive(f: OverflowFixture, taskId: string) {
  const attempt = await f.harness.store.tasks.createAttempt({
    attemptId: f.harness.ids.attemptId(),
    taskId,
    ownerId: "worker-1",
  });
  const lease = await f.harness.store.tasks.acquireLease({
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

async function sendAndDrive(f: OverflowFixture, content: string) {
  const submitted = await f.runner.submitMessage({ chatId: f.chatId, content });
  await drive(f, submitted.runId);
  return submitted;
}

const price = (messages: readonly AiChatMessage[]) =>
  messages.reduce(
    (sum, message) => sum + estimator.estimateMessage(message),
    0,
  );

describe("model context overflow (M0 fault matrix row 20)", () => {
  it("admits at the estimated threshold and refuses one token over, explicitly, with durable history unchanged", async () => {
    // PROBE: price the exact provider-facing list the candidate turn builds.
    const probe = await setup(100_000);
    await sendAndDrive(probe, WARMUP);
    const probeRun = await sendAndDrive(probe, PROBE);
    expect(
      (await probe.harness.store.tasks.getTask(probeRun.runId))?.status,
    ).toBe("completed");
    // The candidate's FIRST provider pass is the list `fitToBudget` priced:
    // system prompt, warm-up replay, and the new user message — capture and
    // price exactly that.
    const facing = probe.mock.requests.find(
      (messages) =>
        messages.at(-1)?.role === "user" && messages.at(-1)?.content === PROBE,
    );
    if (!facing) throw new Error("Probe never sent the candidate request");
    const fullTokens = price(facing);
    // `fitToBudget` drops whole OLD turns and never the current one: the
    // threshold that decides admit-vs-refuse is the cost that trimming cannot
    // remove — the system prompt plus the candidate turn itself.
    const currentOnly = price([
      ...facing.filter((message) => message.role === "system"),
      facing.at(-1)!,
    ]);
    expect(
      facing.some(
        (message) => message.role === "user" && message.content === WARMUP,
      ),
    ).toBe(true);
    expect(currentOnly).toBeLessThan(fullTokens);

    // AT THE THRESHOLD: budget == full estimated cost → admitted with the
    // full eligible ancestry, provider called once, no overflow, no trim.
    const atThreshold = await setup(fullTokens + RESERVE_TOKENS);
    await sendAndDrive(atThreshold, WARMUP);
    const callsBefore = atThreshold.mock.callCount;
    const accepted = await sendAndDrive(atThreshold, PROBE);
    expect(
      (await atThreshold.harness.store.tasks.getTask(accepted.runId))?.status,
    ).toBe("completed");
    // Admitted means the provider ran the turn exactly once.
    expect(atThreshold.mock.callCount).toBe(callsBefore + 1);
    const admittedRequest = [...atThreshold.mock.requests]
      .reverse()
      .find(
        (messages) =>
          messages.at(-1)?.role === "user" &&
          messages.at(-1)?.content === PROBE,
      );
    expect(
      admittedRequest?.some(
        (message) => message.role === "user" && message.content === WARMUP,
      ),
    ).toBe(true);
    const acceptedEvents = (await atThreshold.harness.store.tasks.listEvents(
      accepted.runId,
    )) as AiRunEvent[];
    expect(
      acceptedEvents.some(
        (event) =>
          event.type === "run.failed" &&
          event.data.errorCode === "context_overflow",
      ),
    ).toBe(false);
    expect(
      acceptedEvents.some(
        (event) =>
          event.type === "run.warning" && event.data.code === "context_trimmed",
      ),
    ).toBe(false);

    // ONE TOKEN OVER THE UN-TRIMMABLE THRESHOLD: even dropping every old
    // turn does not fit, so the refusal is explicit and precedes the provider.
    const oneOver = await setup(currentOnly + RESERVE_TOKENS - 1);
    await sendAndDrive(oneOver, WARMUP);
    const historyBefore =
      await oneOver.harness.store.conversations.listMessages(oneOver.chatId);
    const callsBeforeRefusal = oneOver.mock.callCount;

    const rejected = await oneOver.runner.submitMessage({
      chatId: oneOver.chatId,
      content: PROBE,
    });
    let refusal: unknown;
    try {
      await drive(oneOver, rejected.runId);
    } catch (err) {
      refusal = err;
    }

    // Explicit: a coded host error naming the numbers, not a silent drop.
    expect(refusal).toBeInstanceOf(Error);
    const error = refusal as {
      code?: string;
      message?: string;
      details?: Record<string, unknown>;
    };
    expect(error.code).toBe("context_overflow");
    expect(error.message).toMatch(/context/i);
    expect(error.details).toMatchObject({
      estimatedTokens: currentOnly,
      budgetTokens: currentOnly - 1,
      windowTokens: currentOnly + RESERVE_TOKENS - 1,
    });
    // BEFORE any provider call: the refused turn spent nothing.
    expect(oneOver.mock.callCount).toBe(callsBeforeRefusal);

    // Durable log carries the same explicit verdict.
    const events = (await oneOver.harness.store.tasks.listEvents(
      rejected.runId,
    )) as AiRunEvent[];
    const failed = events.find((event) => event.type === "run.failed");
    expect(failed?.type === "run.failed" && failed.data.errorCode).toBe(
      "context_overflow",
    );
    expect(
      (await oneOver.harness.store.tasks.getTask(rejected.runId))?.status,
    ).toBe("failed");

    // Durable history UNCHANGED: every pre-refusal message is still there
    // byte-identical, and the rejected question itself was not silently eaten.
    const historyAfter = await oneOver.harness.store.conversations.listMessages(
      oneOver.chatId,
    );
    const afterById = new Map(
      historyAfter.map((message) => [message.id, message]),
    );
    for (const before of historyBefore)
      expect(JSON.stringify(afterById.get(before.id))).toBe(
        JSON.stringify(before),
      );
    expect(
      historyAfter.some(
        (message) => message.role === "user" && message.content === PROBE,
      ),
    ).toBe(true);

    // Nothing stranded: no human-input record left parked (`pending`) by the
    // refused turn, and capacity is released — the next turn admits, runs and completes.
    for (const record of oneOver.inputs.records.values())
      expect(record.status).not.toBe("pending");
    const followUp = await sendAndDrive(
      oneOver,
      "the disk of context is clear again",
    );
    expect(
      (await oneOver.harness.store.tasks.getTask(followUp.runId))?.status,
    ).toBe("completed");
    expect(oneOver.mock.callCount).toBe(callsBeforeRefusal + 1);
  });

  it("between threshold and overflow, old turns trim with a warning (§8 provider-facing vs full ancestry)", async () => {
    // The middle ground the §8 bullet asks to "compare": a budget too small
    // for the full ancestry but big enough for the current turn admits with
    // a trim warning, and the provider sees the trimmed list — the durable
    // history keeps everything.
    const probe = await setup(100_000);
    await sendAndDrive(probe, WARMUP);
    await sendAndDrive(probe, PROBE);
    const facing = probe.mock.requests.find(
      (messages) =>
        messages.at(-1)?.role === "user" && messages.at(-1)?.content === PROBE,
    );
    if (!facing) throw new Error("Probe never sent the candidate request");
    const currentOnly = price([
      ...facing.filter((message) => message.role === "system"),
      facing.at(-1)!,
    ]);

    const f = await setup(currentOnly + RESERVE_TOKENS);
    await sendAndDrive(f, WARMUP);
    const trimmed = await sendAndDrive(f, PROBE);
    expect((await f.harness.store.tasks.getTask(trimmed.runId))?.status).toBe(
      "completed",
    );
    const events = (await f.harness.store.tasks.listEvents(
      trimmed.runId,
    )) as AiRunEvent[];
    const warnings = events.filter(
      (event) =>
        event.type === "run.warning" && event.data.code === "context_trimmed",
    );
    expect(warnings).toHaveLength(1);
    const providerList = [...f.mock.requests]
      .reverse()
      .find(
        (messages) =>
          messages.at(-1)?.role === "user" &&
          messages.at(-1)?.content === PROBE,
      );
    expect(
      providerList?.some(
        (message) => message.role === "user" && message.content === WARMUP,
      ),
    ).toBe(false);
    // Provider-facing content is trimmed; the DURABLE history is not.
    const history = await f.harness.store.conversations.listMessages(f.chatId);
    expect(
      history.some(
        (message) => message.role === "user" && message.content === WARMUP,
      ),
    ).toBe(true);
  });

  it("far-over requests still refuse with the same coded verdict (regression floor)", async () => {
    const f = await setup(200);
    await sendAndDrive(f, WARMUP);
    const callsBefore = f.mock.callCount;
    const huge = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "y".repeat(40_000),
    });
    await expect(drive(f, huge.runId)).rejects.toMatchObject({
      code: "context_overflow",
    });
    expect(f.mock.callCount).toBe(callsBefore);
  });
});
