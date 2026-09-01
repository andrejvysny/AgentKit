import { describe, expect, it } from "bun:test";
import type { AiRunEvent } from "@agentkit/contracts";
import { MockProviderClient } from "@agentkit/testing";
import {
  TurnRunner,
  UsageDeniedError,
  type UsageAuthorizationDecision,
  type UsageAuthorizationRequest,
  type UsageAuthorizer,
  type UsageRecord,
} from "../src/index.js";
import { createHarness, type TestHarness } from "./fakes.js";

/**
 * A `UsageAuthorizer` that answers from a script and remembers everything it was
 * asked and told — the two halves of the port under test.
 */
class RecordingUsageAuthorizer implements UsageAuthorizer {
  readonly authorizeCalls: UsageAuthorizationRequest[] = [];
  readonly records: UsageRecord[] = [];

  constructor(private readonly answer: UsageAuthorizationDecision) {}

  async authorize(
    request: UsageAuthorizationRequest,
  ): Promise<UsageAuthorizationDecision> {
    this.authorizeCalls.push(request);
    return this.answer;
  }

  async record(usage: UsageRecord): Promise<void> {
    this.records.push(usage);
  }
}

interface Fixture extends TestHarness {
  runner: TurnRunner;
  provider: MockProviderClient;
  chatId: string;
}

async function setup(usage?: UsageAuthorizer): Promise<Fixture> {
  const harness = createHarness();
  const provider = new MockProviderClient();
  provider.emitUsage = true;
  provider.setScript([{ steps: [{ kind: "text", content: "Hi there." }] }]);

  await harness.store.providers.upsertProvider({
    id: "p1",
    label: "Mock",
    kind: "openai-compatible",
    baseUrl: "http://localhost:1234",
    defaultModel: "m1",
    enabled: true,
  });
  await harness.store.settings.updateSettings({ defaultProviderId: "p1" });
  const chat = await harness.store.conversations.createChat({ id: "chat-1" });

  const runner = new TurnRunner({
    store: harness.store,
    taskRunner: harness.taskRunner,
    providerFactory: () => provider,
    contributors: [],
    clock: harness.clock,
    ids: harness.ids,
    ...(usage === undefined ? {} : { usage }),
  });
  return { ...harness, runner, provider, chatId: chat.id };
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

describe("TurnRunner + UsageAuthorizer — refusal", () => {
  it("never reaches the provider, fails the task, and says why in the log", async () => {
    const usage = new RecordingUsageAuthorizer({
      allowed: false,
      reason: "Monthly budget exhausted.",
      retryAfterMs: 60_000,
    });
    const f = await setup(usage);
    const submitted = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "hello",
      model: "m1",
    });

    // The refusal reaches the caller: the queue is what classifies a failed
    // attempt, so swallowing it here would make a denied turn look successful
    // to the runner that scheduled it.
    let thrown: unknown;
    try {
      await drive(f, submitted.runId);
    } catch (err) {
      thrown = err;
    }
    expect(thrown).toBeInstanceOf(UsageDeniedError);
    expect((thrown as UsageDeniedError).code).toBe("usage_denied");
    expect((thrown as UsageDeniedError).message).toContain(
      "Monthly budget exhausted.",
    );

    // The point of asking first: a refusal costs nothing.
    expect(f.provider.callCount).toBe(0);
    expect(usage.authorizeCalls.length).toBe(1);
    expect(usage.records).toEqual([]);

    const events = await eventsOf(f, submitted.runId);
    expect(events.map((event) => event.type)).toEqual(["run.failed"]);
    const failed = events[0];
    if (failed?.type !== "run.failed") throw new Error("expected run.failed");
    expect(failed.data.errorCode).toBe("usage_denied");
    expect(failed.data.errorMessage).toContain("Monthly budget exhausted.");
    // Continues the task's own sequence like every other event on the log.
    expect(failed.seq).toBe(0);

    const task = await f.store.tasks.getTask(submitted.runId);
    expect(task?.status).toBe("failed");
    const attempts = [...f.store.tasks.attempts.values()].filter(
      (attempt) => attempt.taskId === submitted.runId,
    );
    expect(attempts.at(-1)?.status).toBe("failed");
  });

  it("asks with the run's identity and a size estimate off the assembled prompt", async () => {
    const usage = new RecordingUsageAuthorizer({ allowed: false });
    const f = await setup(usage);
    // 40 characters, so the chars/4 estimate is a round 10 and the assertion is
    // about the rule rather than about whatever the fixture happens to say.
    const content = "0123456789012345678901234567890123456789";
    const submitted = await f.runner.submitMessage({
      chatId: f.chatId,
      content,
      model: "m1",
    });
    await drive(f, submitted.runId).catch(() => {});

    expect(usage.authorizeCalls).toEqual([
      {
        runId: submitted.runId,
        chatId: f.chatId,
        providerId: "p1",
        model: "m1",
        estimatedPromptTokens: 10,
      },
    ]);
  });

  it("falls back to a generic reason when the port refuses without one", async () => {
    const usage = new RecordingUsageAuthorizer({ allowed: false });
    const f = await setup(usage);
    const submitted = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "hello",
    });
    await drive(f, submitted.runId).catch(() => {});

    const events = await eventsOf(f, submitted.runId);
    const failed = events[0];
    if (failed?.type !== "run.failed") throw new Error("expected run.failed");
    expect(failed.data.errorMessage).toContain("no reason given");
  });
});

describe("TurnRunner + UsageAuthorizer — approval", () => {
  it("records the provider's own numbers once per run.usage event", async () => {
    const usage = new RecordingUsageAuthorizer({ allowed: true });
    const f = await setup(usage);
    const submitted = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "hello",
      model: "m1",
    });
    await drive(f, submitted.runId);

    expect(f.provider.callCount).toBe(1);
    expect(usage.authorizeCalls.length).toBe(1);
    expect(usage.records.length).toBe(1);

    const record = usage.records[0];
    // The mock's scripted accounting, passed through untouched — the estimate
    // the pass was authorized on plays no part in what gets recorded.
    expect(record).toMatchObject({
      runId: submitted.runId,
      callId: "call-usage-0",
      attempt: 1,
      providerId: "p1",
      model: "m1",
      promptTokens: 10,
      completionTokens: 5,
      totalTokens: 15,
      // The three fields that say WHICH usage report this is. Without them a
      // recorder cannot tell an interim streaming report from the call's
      // settled numbers, and since TurnRunner deliberately reports both, it
      // would have to double-count to be safe.
      finalForCall: true,
      source: "stream",
      // The run loop stamps the iteration over whatever the client said (see
      // core's `run-loop.ts`), and it is THAT value the port has to see.
      step: 1,
    });
    expect(typeof record?.at).toBe("string");

    // The whole event, mirrored: nothing the contract carries about a usage
    // report is dropped on the way to the port.
    const usageEvent = (await eventsOf(f, submitted.runId)).find(
      (event) => event.type === "run.usage",
    );
    if (usageEvent?.type !== "run.usage") throw new Error("expected run.usage");
    expect(record?.finalForCall).toBe(usageEvent.data.finalForCall);
    expect(record?.source).toBe(usageEvent.data.source);
    expect(record?.step).toBe(usageEvent.data.step);

    // Recorded only after the event was durable: a record with no event behind
    // it is a charge nobody can audit.
    const events = await eventsOf(f, submitted.runId);
    expect(events.some((event) => event.type === "run.usage")).toBe(true);

    const task = await f.store.tasks.getTask(submitted.runId);
    expect(task?.status).toBe("completed");
  });

  it("asks again for a retry pass, because a retry bills again", async () => {
    const usage = new RecordingUsageAuthorizer({ allowed: true });
    const f = await setup(usage);
    // An empty first answer is what drives the runner's empty-response retry:
    // two `runChat` invocations, and therefore two authorizations.
    f.provider.setScript([{ steps: [] }, { steps: [] }]);
    const submitted = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "hello",
      model: "m1",
    });
    await drive(f, submitted.runId);

    expect(f.provider.callCount).toBe(2);
    expect(usage.authorizeCalls.length).toBe(2);
  });

  it("refusing the RETRY pass still ends the run, after the first pass ran", async () => {
    let answers = 0;
    const usage: UsageAuthorizer = {
      async authorize() {
        answers += 1;
        return answers === 1
          ? { allowed: true }
          : { allowed: false, reason: "Budget ran out mid-turn." };
      },
      async record() {},
    };
    const f = await setup(usage);
    f.provider.setScript([{ steps: [] }, { steps: [] }]);
    const submitted = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "hello",
    });
    await expect(drive(f, submitted.runId)).rejects.toThrow(UsageDeniedError);

    // The first pass happened and the second did not.
    expect(f.provider.callCount).toBe(1);
    const events = await eventsOf(f, submitted.runId);
    const last = events.at(-1);
    if (last?.type !== "run.failed") throw new Error("expected run.failed");
    expect(last.data.errorCode).toBe("usage_denied");
    // One unbroken sequence: the host-written failure continues the pass's log.
    expect(events.map((event) => event.seq)).toEqual(
      events.map((_event, index) => index),
    );

    const task = await f.store.tasks.getTask(submitted.runId);
    expect(task?.status).toBe("failed");
  });
});

describe("TurnRunner without a UsageAuthorizer", () => {
  it("runs the turn unchanged — an unwired port is no enforcement at all", async () => {
    const f = await setup();
    const submitted = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "hello",
      model: "m1",
    });
    await drive(f, submitted.runId);

    expect(f.provider.callCount).toBe(1);
    const task = await f.store.tasks.getTask(submitted.runId);
    expect(task?.status).toBe("completed");
    const answer = f.store.conversations.messages.find(
      (message) => message.id === submitted.assistantMessageId,
    );
    expect(answer?.content).toBe("Hi there.");
    // The usage event is still logged; only the port call is absent.
    const events = await eventsOf(f, submitted.runId);
    expect(events.some((event) => event.type === "run.usage")).toBe(true);
  });
});
