/**
 * The generalization, end to end: a task kind that has nothing to do with chat,
 * running on the same durable machinery a chat turn runs on.
 *
 * `e2e-vertical-slice.test.ts` proves the chat stack. This file proves the claim
 * that made the stack worth generalizing — that leases, attempts, the seq'd
 * event log, cancellation, idempotent submission and the retry/dead-letter split
 * are properties of TASKS, not of turns. A host that writes an executor and a
 * kind string gets all of it, and this file is the worked example of doing so:
 * {@link EchoExecutor} is ~15 lines and touches nothing but public ports.
 *
 * The last scenario runs a `chat.turn` and a `demo.echo` task through ONE runner
 * and ONE registry, which is the property no single-kind test can show.
 */
import { describe, expect, it } from "bun:test";
import Ajv from "ajv";
import {
  TaskEventEnvelopeSchema,
  type TaskEventEnvelope,
} from "@agentkit/contracts";
import {
  CHAT_TURN_TASK_KIND,
  ChatTurnExecutor,
  ExecutorRegistry,
  TaskService,
  TurnRunner,
  createDispatchingWorker,
  createTaskEventWriter,
  type AssistantStore,
  type IdGenerator,
  type TaskExecutionContext,
  type TaskExecutor,
} from "@agentkit/host";
import { MockProviderClient } from "@agentkit/testing";
import {
  MemoryAssistantStore,
  SingleProcessTaskRunner,
  SqliteAssistantStore,
} from "../src/index.js";
import {
  createTestClock,
  settle,
  waitFor,
  type TestClock,
} from "./support/task-runner-harness.js";

const DEMO_ECHO_KIND = "demo.echo";
/** The fan-out trio: a task that spawns leaves, the leaves, and what follows them. */
const DEMO_FANOUT_KIND = "demo.fanout";
const DEMO_LEAF_KIND = "demo.leaf";
const DEMO_CONTINUATION_KIND = "demo.continuation";
const CHAT_ID = "chat-generic";

// The schema value round-tripped through JSON is the plain JSON Schema Ajv
// wants; importing TypeBox here just to name its type would add a dependency
// this workspace-private package has no other reason to hold.
const validateEnvelope = new Ajv({ strict: false, allErrors: true }).compile(
  JSON.parse(JSON.stringify(TaskEventEnvelopeSchema)) as object,
);

/** Counter-based ids, so an assertion can name the id it expects. */
function createSequentialIds(): IdGenerator {
  const counters = new Map<string, number>();
  const next = (kind: string): string => {
    const n = (counters.get(kind) ?? 0) + 1;
    counters.set(kind, n);
    return `${kind}-${n}`;
  };
  return {
    taskId: () => next("task"),
    attemptId: () => next("att"),
    eventId: () => next("evt"),
    proposalId: () => next("prp"),
    operationId: () => next("op"),
    messageId: () => next("msg"),
  };
}

/**
 * A whole non-chat executor.
 *
 * RESULT CONVENTION: the terminal event carries the result. There is no
 * `result` column on a task — the event log already is the durable, ordered,
 * replayable record of what happened, and a second place to write the answer
 * would be a second thing to keep consistent with it. A consumer reads the last
 * event; a UI that was streaming already has it.
 *
 * The task is NOT landed here: the executor returns, and the runner's
 * `settleResolved` transitions it `completed` and ends the attempt. TurnRunner
 * does the opposite (it lands its own task, because it owns the cancelled/failed
 * distinction); both are supported, and this is the simpler half a host starts
 * from.
 */
class EchoExecutor implements TaskExecutor {
  readonly kind = DEMO_ECHO_KIND;
  /** Executions that actually happened — the idempotency witness. */
  invocations = 0;

  constructor(
    private readonly store: AssistantStore,
    private readonly clock: TestClock,
    private readonly ids: IdGenerator,
  ) {}

  async execute(ctx: TaskExecutionContext): Promise<void> {
    this.invocations += 1;
    const writer = createTaskEventWriter({
      tasks: this.store.tasks,
      taskId: ctx.task.taskId,
      attemptId: ctx.attemptId,
      leaseToken: ctx.leaseToken,
      clock: this.clock,
      ids: this.ids,
    });
    const text = String(ctx.task.payload["text"] ?? "");
    await writer.emit({ type: "demo.started", data: { text } });
    await writer.emit({
      type: "demo.completed",
      data: { echoed: text.toUpperCase() },
    });
  }
}

/**
 * The fan-out half of the subagent pattern: spawn two leaves, then a
 * continuation that `dependsOn` both.
 *
 * The parent does NOT wait for its children — it returns as soon as they are
 * submitted, and its own task completes. That is the point of expressing "and
 * then, once they are done" as a third task with a dependency rather than as an
 * await: the parent holds no lease, no scope and no concurrency slot while the
 * children run, so a crash in between loses nothing and the queue is free.
 */
class FanOutExecutor implements TaskExecutor {
  readonly kind = DEMO_FANOUT_KIND;

  constructor(private readonly leafPayloads: Record<string, unknown>[]) {}

  async execute(ctx: TaskExecutionContext): Promise<void> {
    const spawnChild = ctx.spawnChild;
    if (!spawnChild) {
      throw new Error(`${DEMO_FANOUT_KIND} needs a dispatcher with a TaskService`);
    }
    const children = [];
    for (const [index, payload] of this.leafPayloads.entries()) {
      children.push(
        await spawnChild({
          taskId: `task-leaf-${index}`,
          kind: DEMO_LEAF_KIND,
          scopeId: `scope-leaf-${index}`,
          payload,
        }),
      );
    }
    await spawnChild({
      taskId: "task-continuation",
      kind: DEMO_CONTINUATION_KIND,
      scopeId: "scope-continuation",
      payload: {},
      dependsOn: children.map((child) => child.taskId),
    });
  }
}

/** Records the order work actually ran in; optionally blows up instead. */
class OrderedExecutor implements TaskExecutor {
  constructor(
    readonly kind: string,
    private readonly order: string[],
  ) {}

  async execute(ctx: TaskExecutionContext): Promise<void> {
    this.order.push(ctx.task.taskId);
    if (ctx.task.payload["explode"] === true) {
      // Unrecognised by the classifier ⇒ terminal, so the leaf fails on its
      // first attempt rather than retrying into the continuation's timeout.
      throw new Error("leaf work exploded");
    }
  }
}

interface Env {
  clock: TestClock;
  store: AssistantStore;
  taskRunner: SingleProcessTaskRunner;
  registry: ExecutorRegistry;
  tasks: TaskService;
  echo: EchoExecutor;
  turnRunner: TurnRunner;
  provider: MockProviderClient;
  /** Task ids in the order the fan-out executors actually ran them. */
  ranInOrder: string[];
  close(): void;
}

/**
 * The wiring a host copies for a multi-kind deployment: one store, one queue,
 * one registry with an executor per kind, and one dispatching worker over all of
 * them.
 */
function createEnv(
  backing: "memory" | "sqlite",
  /** Folded into the SECOND leaf's payload — `{ explode: true }` fails it. */
  leafOverrides: Record<string, unknown> = {},
): Env {
  const clock = createTestClock();
  const ids = createSequentialIds();
  const sqlite =
    backing === "sqlite"
      ? new SqliteAssistantStore(":memory:", { clock, ids })
      : null;
  const store: AssistantStore =
    sqlite ?? new MemoryAssistantStore({ clock, ids });

  const taskRunner = new SingleProcessTaskRunner({
    store,
    clock,
    pollMs: 5,
    // Far past any test's real lifetime: renewal must not rescue or expire a
    // lease behind a test's back.
    heartbeatMs: 60_000,
  });

  const provider = new MockProviderClient();
  provider.setScript([{ steps: [{ kind: "text", content: "Hello there." }] }]);
  const turnRunner = new TurnRunner({
    store,
    taskRunner,
    providerFactory: () => provider,
    contributors: [],
    clock,
    ids,
  });

  const echo = new EchoExecutor(store, clock, ids);
  const registry = new ExecutorRegistry();
  registry.register(echo);
  registry.register(new ChatTurnExecutor(turnRunner));

  const ranInOrder: string[] = [];
  registry.register(
    new FanOutExecutor([{ text: "left" }, { text: "right", ...leafOverrides }]),
  );
  registry.register(new OrderedExecutor(DEMO_LEAF_KIND, ranInOrder));
  registry.register(new OrderedExecutor(DEMO_CONTINUATION_KIND, ranInOrder));

  return {
    clock,
    store,
    taskRunner,
    registry,
    tasks: new TaskService({ store, taskRunner, ids, clock }),
    echo,
    turnRunner,
    provider,
    ranInOrder,
    close: () => sqlite?.close(),
  };
}

async function startWorker(env: Env): Promise<{ stop: () => Promise<void> }> {
  return env.taskRunner.startWorker(
    createDispatchingWorker(env.registry, {
      store: env.store,
      clock: env.clock,
      // What turns `spawnChild` on for every executor this worker dispatches:
      // a spawn is a submit, and a submit needs the queue.
      taskService: env.tasks,
    }),
    { concurrency: 2, ownerId: "owner-generic" },
  );
}

async function seedChatState(env: Env): Promise<void> {
  await env.store.providers.upsertProvider({
    id: "p1",
    label: "Mock",
    kind: "openai-compatible",
    baseUrl: "http://localhost:1234",
    defaultModel: "m1",
    enabled: true,
  });
  await env.store.settings.updateSettings({ defaultProviderId: "p1" });
  await env.store.conversations.createChat({ id: CHAT_ID });
}

async function statusOf(env: Env, taskId: string): Promise<string> {
  return (await env.store.tasks.getTask(taskId))?.status ?? "missing";
}

function isTerminal(status: string): boolean {
  return status === "completed" || status === "failed" || status === "cancelled";
}

async function waitForTerminal(env: Env, taskId: string): Promise<void> {
  await waitFor(
    async () => isTerminal(await statusOf(env, taskId)),
    `${taskId} to reach a terminal state`,
  );
}

/** Gapless from 0, and every event is a valid task-event envelope. */
function expectValidStream(events: TaskEventEnvelope[]): void {
  expect(events.length).toBeGreaterThan(0);
  expect(events.map((event) => event.seq)).toEqual(
    events.map((_, index) => index),
  );
  for (const event of events) {
    const ok = validateEnvelope(event);
    if (!ok) throw new Error(JSON.stringify(validateEnvelope.errors));
    expect(ok).toBe(true);
  }
  expect(new Set(events.map((event) => event.eventId)).size).toBe(
    events.length,
  );
}

for (const backing of ["memory", "sqlite"] as const) {
  describe(`generic task e2e (${backing}) — a non-chat kind on the chat machinery`, () => {
    it("runs a demo.echo task to completed, with its own event vocabulary in one unbroken stream", async () => {
      const env = createEnv(backing);
      const handle = await startWorker(env);
      try {
        const submitted = await env.tasks.submitTask({
          taskId: "task-echo",
          kind: DEMO_ECHO_KIND,
          scopeId: "scope-echo",
          payload: { text: "hello" },
        });
        expect(submitted.kind).toBe(DEMO_ECHO_KIND);
        await waitForTerminal(env, "task-echo");

        const task = await env.store.tasks.getTask("task-echo");
        expect(task?.status).toBe("completed");
        expect(task?.attemptCount).toBe(1);
        expect(task?.finishedAt).toBeDefined();
        expect(task?.deadLetteredAt).toBeUndefined();
        expect(env.echo.invocations).toBe(1);

        const events = await env.store.tasks.listEvents("task-echo");
        expectValidStream(events);
        expect(events.map((event) => event.type)).toEqual([
          "demo.started",
          "demo.completed",
        ]);
        // The terminal event carries the result — see the convention on
        // EchoExecutor.
        const terminal = events.at(-1) as unknown as {
          data: { echoed: string };
        };
        expect(terminal.data).toEqual({ echoed: "HELLO" });
        // Every event is attributed to the attempt that wrote it, which is what
        // makes a retried task's log readable.
        const attemptIds = new Set(events.map((event) => event.attemptId));
        expect(attemptIds.size).toBe(1);
      } finally {
        await handle.stop();
        env.close();
      }
    });

    it("is idempotent on resubmit: the same taskId never executes twice", async () => {
      const env = createEnv(backing);
      const handle = await startWorker(env);
      try {
        const input = {
          taskId: "task-once",
          kind: DEMO_ECHO_KIND,
          scopeId: "scope-echo",
          payload: { text: "once" },
        };
        await env.tasks.submitTask(input);
        await waitForTerminal(env, "task-once");
        expect(env.echo.invocations).toBe(1);

        // The redelivery a retried HTTP call or a replayed message produces.
        const again = await env.tasks.submitTask({
          ...input,
          payload: { text: "twice" },
        });
        expect(again.taskId).toBe("task-once");
        expect(again.payload).toEqual({ text: "once" });

        // Give the claim loop several poll cycles to prove it does NOT re-run.
        // `settle`, not `waitFor`: the task is already `completed`, so a
        // predicate that is true on its first evaluation returns immediately and
        // asserts nothing about what the loop does NEXT — which is the whole
        // claim being made here.
        await settle();
        expect(await statusOf(env, "task-once")).toBe("completed");
        expect(env.echo.invocations).toBe(1);
        expect(
          (await env.store.tasks.getTask("task-once"))?.attemptCount,
        ).toBe(1);
        const events = await env.store.tasks.listEvents("task-once");
        expect(events.map((event) => event.type)).toEqual([
          "demo.started",
          "demo.completed",
        ]);
      } finally {
        await handle.stop();
        env.close();
      }
    });

    it("cancels a queued task before any executor sees it", async () => {
      const env = createEnv(backing);
      const handle = await startWorker(env);
      try {
        // Not claimable yet: `availableAt` in the future keeps it queued
        // deterministically, with no race against the dispatch loop.
        await env.tasks.submitTask({
          taskId: "task-cancel",
          kind: DEMO_ECHO_KIND,
          scopeId: "scope-cancel",
          payload: { text: "never" },
          availableAt: new Date(Date.now() + 3_600_000).toISOString(),
        });
        expect(await statusOf(env, "task-cancel")).toBe("queued");

        await env.taskRunner.requestCancel("task-cancel");

        const task = await env.store.tasks.getTask("task-cancel");
        expect(task?.status).toBe("cancelled");
        expect(task?.finishedAt).toBeDefined();
        expect(env.echo.invocations).toBe(0);
        expect(await env.store.tasks.listEvents("task-cancel")).toEqual([]);
      } finally {
        await handle.stop();
        env.close();
      }
    });

    it("fans out two children and runs a continuation that depends on both", async () => {
      const env = createEnv(backing);
      const handle = await startWorker(env);
      try {
        await env.tasks.submitTask({
          taskId: "task-parent",
          kind: DEMO_FANOUT_KIND,
          scopeId: "scope-parent",
          payload: {},
        });
        await waitForTerminal(env, "task-continuation");

        // Everything landed, and the parent did not have to stay alive for it.
        expect(await statusOf(env, "task-parent")).toBe("completed");
        expect(await statusOf(env, "task-leaf-0")).toBe("completed");
        expect(await statusOf(env, "task-leaf-1")).toBe("completed");
        expect(await statusOf(env, "task-continuation")).toBe("completed");

        // The ordering claim: the continuation ran, and it ran LAST. Without
        // the dependency gate the queue would happily have started it first —
        // it is in its own scope with nothing else holding it back.
        expect(env.ranInOrder.at(-1)).toBe("task-continuation");
        expect(env.ranInOrder.slice(0, -1).sort()).toEqual([
          "task-leaf-0",
          "task-leaf-1",
        ]);

        // Lineage is recorded on every spawned task, so the whole branch is
        // discoverable from the parent.
        const children = await env.store.tasks.listChildren("task-parent");
        expect(children.map((task) => task.taskId).sort()).toEqual([
          "task-continuation",
          "task-leaf-0",
          "task-leaf-1",
        ]);
        expect(
          (await env.store.tasks.getTask("task-continuation"))?.dependsOn,
        ).toEqual(["task-leaf-0", "task-leaf-1"]);
      } finally {
        await handle.stop();
        env.close();
      }
    });

    it("fails the continuation without running it when one child fails", async () => {
      const env = createEnv(backing, { explode: true });
      const handle = await startWorker(env);
      try {
        await env.tasks.submitTask({
          taskId: "task-parent",
          kind: DEMO_FANOUT_KIND,
          scopeId: "scope-parent",
          payload: {},
        });
        await waitForTerminal(env, "task-continuation");

        expect(await statusOf(env, "task-leaf-0")).toBe("completed");
        expect(await statusOf(env, "task-leaf-1")).toBe("failed");

        const continuation = await env.store.tasks.getTask("task-continuation");
        expect(continuation?.status).toBe("failed");
        expect(continuation?.error).toBe("dependency_failed: task-leaf-1");
        // Settled by the claim, never dispatched: no attempt, no executor run,
        // no events. A continuation that ran anyway would be acting on half a
        // result.
        expect(continuation?.attemptCount).toBe(0);
        expect(env.ranInOrder).not.toContain("task-continuation");
        expect(await env.store.tasks.listEvents("task-continuation")).toEqual(
          [],
        );
        // The failed leaf burned exactly one attempt and was not dead-lettered.
        expect(
          (await env.store.tasks.getTask("task-leaf-1"))?.deadLetteredAt,
        ).toBeUndefined();
      } finally {
        await handle.stop();
        env.close();
      }
    });

    it("fails a task of an unregistered kind WITHOUT dead-lettering it", async () => {
      const env = createEnv(backing);
      const handle = await startWorker(env);
      try {
        await env.tasks.submitTask({
          taskId: "task-orphan",
          kind: "nobody.registered",
          scopeId: "scope-orphan",
          payload: {},
        });
        await waitForTerminal(env, "task-orphan");

        const task = await env.store.tasks.getTask("task-orphan");
        expect(task?.status).toBe("failed");
        expect(task?.error).toContain("executor_not_found");
        expect(task?.error).toContain("nobody.registered");
        // Dead-letter is reserved for poison — a queue that must stop being fed.
        // A kind nobody registered is a wiring mistake with a clean diagnosis,
        // and it burns exactly one attempt.
        expect(task?.deadLetteredAt).toBeUndefined();
        expect(task?.deadLetterReason).toBeUndefined();
        expect(task?.attemptCount).toBe(1);
      } finally {
        await handle.stop();
        env.close();
      }
    });
  });
}

describe("generic task e2e — two kinds, one runner", () => {
  it("completes a chat.turn and a demo.echo task in different scopes under one registry", async () => {
    const env = createEnv("memory");
    await seedChatState(env);
    const handle = await startWorker(env);
    try {
      // The chat turn goes in through TurnRunner (which writes its messages and
      // its task in one transaction); the echo task goes in through TaskService.
      const turn = await env.turnRunner.submitMessage({
        chatId: CHAT_ID,
        content: "Hi",
      });
      await env.tasks.submitTask({
        taskId: "task-echo-parallel",
        kind: DEMO_ECHO_KIND,
        scopeId: "scope-echo",
        payload: { text: "side quest" },
      });

      await waitForTerminal(env, turn.runId);
      await waitForTerminal(env, "task-echo-parallel");

      const chatTask = await env.store.tasks.getTask(turn.runId);
      expect(chatTask?.status).toBe("completed");
      expect(chatTask?.kind).toBe(CHAT_TURN_TASK_KIND);
      expect(chatTask?.scopeId).toBe(CHAT_ID);
      expect(chatTask?.payload["chatId"]).toBe(CHAT_ID);

      const echoTask = await env.store.tasks.getTask("task-echo-parallel");
      expect(echoTask?.status).toBe("completed");
      expect(echoTask?.kind).toBe(DEMO_ECHO_KIND);

      // Two vocabularies, two independent gapless streams, one store.
      const chatEvents = await env.store.tasks.listEvents(turn.runId);
      expectValidStream(chatEvents);
      expect(chatEvents[0]?.type).toBe("run.started");
      const echoEvents = await env.store.tasks.listEvents(
        "task-echo-parallel",
      );
      expectValidStream(echoEvents);
      expect(echoEvents[0]?.type).toBe("demo.started");

      // The answer landed in the placeholder the submit transaction wrote.
      const messages = await env.store.conversations.listMessages(CHAT_ID);
      expect(
        messages.find((message) => message.id === turn.assistantMessageId)
          ?.content,
      ).toBe("Hello there.");
      expect(env.echo.invocations).toBe(1);
    } finally {
      await handle.stop();
      env.close();
    }
  });
});
