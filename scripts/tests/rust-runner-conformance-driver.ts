import { MemoryAssistantStore } from "@agentkit/adapters-memory";
import { SqliteAssistantStore } from "@agentkit/adapters-sqlite";
import {
  createTaskEventWriter,
  type AssistantStore,
  type IdGenerator,
  type TaskExecution,
  type WorkerHandle,
} from "@agentkit/host";
import { SingleProcessTaskRunner } from "@agentkit/runner-local";
import { clock, type MutableClock } from "./rust-store-conformance-driver.js";
import type { CaseName, Json } from "./rust-runner-conformance-model.js";
import { observeAttempts } from "./rust-runner-conformance-observer.js";

export interface Deferred {
  promise: Promise<void>;
  resolve(): void;
}
export function deferred(): Deferred {
  let resolve = (): void => {};
  const promise = new Promise<void>((done) => {
    resolve = done;
  });
  return { promise, resolve };
}
export const pause = (ms = 5): Promise<void> =>
  new Promise((done) => setTimeout(done, ms));
export async function until(
  check: () => boolean | Promise<boolean>,
  label: string,
): Promise<void> {
  const deadline = Date.now() + 10_000;
  while (!(await check())) {
    if (Date.now() >= deadline)
      throw new Error(`Runner conformance timed out: ${label}`);
    await pause();
  }
}

export interface State {
  effects: number;
  staleDenied: boolean;
  verification: Json;
  entered: Deferred;
  release: Deferred;
  done: Deferred;
}
export const state = (): State => ({
  effects: 0,
  staleDenied: false,
  verification: null,
  entered: deferred(),
  release: deferred(),
  done: deferred(),
});
export interface Environment {
  store: AssistantStore;
  runner: SingleProcessTaskRunner;
  time: MutableClock;
  ids: IdGenerator;
  attempts(): string[];
  close(): void;
}

export async function environment(
  adapter: "memory" | "sqlite",
  path = ":memory:",
  at = 1000,
): Promise<Environment> {
  const time = clock();
  time.at = at;
  let serial = 0;
  const next = (): string => `fault-${at}-${++serial}`;
  const ids: IdGenerator = {
    taskId: next,
    attemptId: next,
    eventId: next,
    proposalId: next,
    operationId: next,
    messageId: next,
    chatId: next,
  };
  const store =
    adapter === "memory"
      ? new MemoryAssistantStore({ clock: time, ids, leaseTtlMs: 1000 })
      : new SqliteAssistantStore(path, { clock: time, ids, leaseTtlMs: 1000 });
  const attempts = observeAttempts(
    store,
    adapter === "sqlite" ? path : ":memory:",
  );
  const runner = new SingleProcessTaskRunner({
    store,
    clock: time,
    recoveryMode: "manual",
    leaseTtlMs: 1000,
    heartbeatMs: 100,
    pollMs: 2,
    maxAttempts: 3,
    retryBackoff: { baseMs: 200, maxMs: 2000, jitterRatio: 0 },
  });
  await runner.recoverWithReport();
  return {
    store,
    runner,
    time,
    ids,
    attempts,
    close: () => {
      if (store instanceof SqliteAssistantStore) store.close();
    },
  };
}

export async function submit(env: Environment): Promise<void> {
  await env.store.tasks.createTask({
    taskId: "run",
    kind: "fault",
    scopeId: "scope",
    payload: {},
  });
  await env.runner.enqueue({ taskId: "run", scopeId: "scope" });
}

export async function snapshot(
  env: Environment,
  witness: State,
  label: string,
  extra: Record<string, Json> = {},
): Promise<Json> {
  const task = await env.store.tasks.getTask("run");
  if (!task) throw new Error("Missing conformance task");
  const attempts = env.attempts();
  const events = await env.store.tasks.listEvents("run");
  for (const event of events) {
    if (
      event.contractVersion !== "0.7.0" ||
      typeof event.eventId !== "string" ||
      typeof event.attemptId !== "string" ||
      Number.isNaN(Date.parse(event.timestamp))
    )
      throw new Error("Invalid runtime event envelope");
  }
  return JSON.parse(
    JSON.stringify({
      label,
      status: task.status,
      attemptCount: task.attemptCount,
      attempts,
      effects: witness.effects,
      events: events.map((entry) => ({
        seq: entry.seq,
        type: entry.type,
        data: "data" in entry ? entry.data : null,
      })),
      poisonCount: task.poisonCount,
      deadLetter: task.deadLetteredAt !== undefined,
      failureReason:
        task.status === "failed" ? (task.error?.split(":")[0] ?? null) : null,
      ...extra,
    }),
  ) as Json;
}

async function emit(
  env: Environment,
  execution: TaskExecution,
  type: string,
  data: Json,
): Promise<void> {
  await createTaskEventWriter({
    tasks: env.store.tasks,
    taskId: "run",
    attemptId: execution.attemptId,
    leaseToken: execution.leaseToken,
    clock: env.time,
    ids: env.ids,
  }).emit({ type, data });
}
const aborted = (): Error =>
  Object.assign(new Error("cancelled"), { name: "AbortError" });
const safeFailure = (): Error =>
  Object.assign(new Error("safe pre-dispatch"), { retryable: true });

export function worker(
  env: Environment,
  witness: State,
  scenario: CaseName,
): { execute(execution: TaskExecution): Promise<void> } {
  return {
    async execute(execution) {
      try {
        const task = await env.store.tasks.getTask("run");
        if (!task) throw new Error("Missing executable task");
        if (scenario === "verification")
          return await verify(env, witness, execution);
        await emit(env, execution, "fault.started", {
          attempt: task.attemptCount,
        });
        if (
          (scenario === "safe_retry" || scenario === "crash_recovery") &&
          task.attemptCount === 1
        )
          throw safeFailure();
        if (scenario === "stale_publication") {
          await stalePublication(env, witness, execution);
        }
        witness.effects += 1;
        if (scenario === "cancel_after_commit") {
          witness.entered.resolve();
          await witness.release.promise;
        }
        await emit(env, execution, "fault.committed", {
          effect: witness.effects,
        });
        if (scenario === "cancel_after_commit" && execution.signal.aborted)
          throw aborted();
        if (scenario === "uncertain_write" && task.attemptCount === 1)
          throw new Error("ECONNRESET after domain commit");
      } finally {
        witness.done.resolve();
      }
    },
  };
}

async function stalePublication(
  env: Environment,
  witness: State,
  execution: TaskExecution,
): Promise<never> {
  witness.entered.resolve();
  await witness.release.promise;
  try {
    await emit(env, execution, "fault.committed", { effect: 1 });
  } catch (error) {
    if (
      !(
        typeof error === "object" &&
        error !== null &&
        "code" in error &&
        error.code === "lease_lost"
      )
    )
      throw error;
    witness.staleDenied = true;
    throw error;
  }
  throw new Error("Stale worker published an event");
}

async function verify(
  env: Environment,
  witness: State,
  execution: TaskExecution,
): Promise<void> {
  witness.effects += 1;
  await emit(env, execution, "fault.committed", { effect: witness.effects });
  const task = await env.store.tasks.getTask("run");
  const events = await env.store.tasks.listEvents("run");
  witness.verification = {
    status: task?.status ?? null,
    eventTypes: events.map((entry) => entry.type),
  };
  await emit(env, execution, "run.verification", {
    pass: 0,
    status: "pass",
    deficiencies: [],
  });
}

export async function terminal(env: Environment): Promise<void> {
  await until(
    async () =>
      ["completed", "failed", "cancelled"].includes(
        (await env.store.tasks.getTask("run"))?.status ?? "",
      ),
    "terminal task",
  );
}

export async function runRunnerScenario(
  adapter: "memory" | "sqlite",
  scenario: CaseName,
): Promise<Json[]> {
  const env = await environment(adapter);
  const witness = state();
  await submit(env);
  const handle = await env.runner.startWorker(worker(env, witness, scenario), {
    concurrency: 1,
  });
  const transcript: Json[] = [];
  try {
    if (await driveBoundary(env, witness, scenario, handle, transcript))
      return transcript;
    await terminal(env);
    await handle.stop();
    transcript.push(
      await snapshot(
        env,
        witness,
        "settled",
        scenario === "verification"
          ? { verification: witness.verification }
          : {},
      ),
    );
    return transcript;
  } finally {
    witness.release.resolve();
    await handle.stop();
    env.close();
  }
}

async function driveBoundary(
  env: Environment,
  witness: State,
  scenario: CaseName,
  handle: WorkerHandle,
  transcript: Json[],
): Promise<boolean> {
  if (scenario === "safe_retry" || scenario === "uncertain_write") {
    await driveRetry(env, witness, scenario, transcript);
  } else if (scenario === "cancel_after_commit") {
    await witness.entered.promise;
    transcript.push(await snapshot(env, witness, "committed_before_cancel"));
    await env.runner.requestCancel("run");
    witness.release.resolve();
  } else if (scenario === "stale_publication") {
    await witness.entered.promise;
    await env.store.tasks.acquireLease({
      taskId: "run",
      attemptId: "replacement",
      ownerId: "replacement",
      ttlMs: 1000,
    });
    witness.release.resolve();
    await witness.done.promise;
    await handle.stop();
    transcript.push(
      await snapshot(env, witness, "stale_denied", {
        staleDenied: witness.staleDenied,
      }),
    );
    return true;
  }
  return false;
}

async function driveRetry(
  env: Environment,
  witness: State,
  scenario: CaseName,
  transcript: Json[],
): Promise<void> {
  await until(
    () => env.runner.retryDeadline("run") === 1200,
    "retry reservation",
  );
  if (scenario === "safe_retry") {
    const task = await env.store.tasks.getTask("run");
    const available = task?.availableAt ? Date.parse(task.availableAt) : null;
    transcript.push(
      await snapshot(env, witness, "backoff", {
        retryDue: env.runner.retryDeadline("run") ?? null,
        durableRetryDue:
          available !== null && available > 1000 ? available : null,
      }),
    );
    env.time.at = 1199;
    await pause(10);
    transcript.push(await snapshot(env, witness, "before_due"));
  }
  env.time.at = 1200;
}
