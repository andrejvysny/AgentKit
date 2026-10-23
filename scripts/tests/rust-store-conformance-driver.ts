import type { TaskEventEnvelope } from "@agentkit/contracts";
import type { AssistantStore, ClaimedTask } from "@agentkit/host";
import { SingleProcessTaskRunner } from "@agentkit/runner-local";
import type {
  Json,
  EventInput,
  Operation,
  Scenario,
} from "./rust-store-conformance-model.js";

export interface MutableClock {
  at: number;
  now(): Date;
  nowIso(): string;
}
export const clock = (): MutableClock => ({
  at: 1000,
  now() {
    return new Date(this.at);
  },
  nowIso() {
    return this.now().toISOString();
  },
});
const json = (value: unknown): Json =>
  JSON.parse(JSON.stringify(value)) as Json;

export function event(seq: number, id: string): TaskEventEnvelope {
  return {
    seq,
    eventId: id,
    type: "task.conformance",
    timestamp: "2026-10-22T12:00:00.000Z",
    contractVersion: "0.7.0",
  };
}

function wireEvent(entry: EventInput): TaskEventEnvelope {
  const base = event(entry.seq, entry.id);
  return {
    ...base,
    type: entry.type ?? base.type,
    timestamp: entry.timestamp ?? base.timestamp,
    contractVersion: entry.contractVersion ?? base.contractVersion,
  };
}

/** Only known, equivalent contract failures normalize; unexpected failures escape. */
function errorResult(error: unknown): Json {
  if (typeof error === "object" && error !== null && "code" in error) {
    const code = error.code;
    if (
      typeof code === "string" &&
      [
        "unknown_dependency",
        "duplicate_task",
        "chat_busy",
        "seq_conflict",
        "lease_lost",
        "invalid_import",
      ].includes(code)
    ) {
      return { error: code };
    }
  }
  throw error;
}

export async function runScenario(
  store: AssistantStore,
  time: MutableClock,
  scenario: Scenario,
): Promise<Json[]> {
  const leases = new Map<string, ClaimedTask>();
  const runner = new SingleProcessTaskRunner({
    store,
    clock: time,
    recoveryMode: "manual",
  });
  const transcript: Json[] = [];
  for (const step of scenario.steps) {
    time.at = step.at;
    try {
      transcript.push(await operate(store, time, runner, leases, step.action));
    } catch (error) {
      transcript.push(errorResult(error));
    }
  }
  return transcript;
}

type Action<K extends Operation["op"]> = Extract<Operation, { op: K }>;
async function submit(
  store: AssistantStore,
  action: Action<"submit">,
): Promise<Json> {
  await store.tasks.createTask({
    taskId: action.id,
    scopeId: action.scope,
    kind: action.kind ?? "test",
    payload: { value: action.id },
    priority: action.priority,
    ...(action.depends ? { dependsOn: action.depends } : {}),
    ...(action.parent ? { parentTaskId: action.parent } : {}),
    ...(action.available !== undefined
      ? { availableAt: new Date(action.available).toISOString() }
      : {}),
    ...(action.exclusive ? { exclusiveScope: true } : {}),
  });
  return { ok: true };
}
async function claim(
  store: AssistantStore,
  time: MutableClock,
  leases: Map<string, ClaimedTask>,
  action: Action<"claim">,
): Promise<Json> {
  const claimed = await store.tasks.claimNext({
    ownerId: "conformance",
    now: time.now(),
    scopesBusy: action.busy ?? [],
    ...(action.kinds ? { kinds: action.kinds } : {}),
  });
  if (claimed) leases.set(action.slot, claimed);
  return claimed?.task.taskId ?? null;
}
async function writeLease(
  store: AssistantStore,
  time: MutableClock,
  leases: Map<string, ClaimedTask>,
  action: Action<"append" | "settle">,
): Promise<Json> {
  if (action.op === "append") {
    const { task, lease } = leases.get(action.slot)!;
    await store.tasks.appendEvents(task.taskId, action.events.map(wireEvent), {
      leaseToken: lease.leaseToken,
    });
  } else {
    const { task, lease, attempt } = leases.get(action.slot)!;
    await store.tasks.transitionTask(
      task.taskId,
      ["running"],
      action.status,
      { finishedAt: time.nowIso() },
      { leaseToken: lease.leaseToken },
    );
    await store.tasks.endAttempt({
      attemptId: attempt.attemptId,
      status: action.status,
      leaseToken: lease.leaseToken,
    });
    await store.tasks.releaseLease(lease.leaseToken);
  }
  return { ok: true };
}
async function importConversation(
  store: AssistantStore,
  action: Action<"import">,
): Promise<Json> {
  await store.conversations.importConversation({
    chat: {
      id: action.chatId,
      title: "Migrated",
      metadata: { source: "conformance" },
    },
    messages: action.messages.map((message) => ({
      id: message.id,
      role: message.role,
      content: message.content,
      ...(message.parentId ? { parentMessageId: message.parentId } : {}),
      active: message.active,
      ...(message.internal ? { internal: true } : {}),
    })),
  });
  return { ok: true };
}
async function readImport(
  store: AssistantStore,
  action: Action<"readImport">,
): Promise<Json> {
  const messages = await Promise.all(
    action.ids.map((id) => store.conversations.getMessage(id)),
  );
  return json({
    exists: (await store.conversations.getChat(action.chatId)) !== null,
    messages: messages
      .filter((message) => message !== null)
      .map((message) => ({
        id: message.id,
        role: message.role,
        content: message.content,
        parentId: message.parentMessageId ?? null,
        active: message.active,
        metadata: message.metadata,
        orderKey: message.orderKey,
        depth: message.depth,
        branchIndex: message.branchIndex,
      })),
  });
}
async function operate(
  store: AssistantStore,
  time: MutableClock,
  runner: SingleProcessTaskRunner,
  leases: Map<string, ClaimedTask>,
  action: Operation,
): Promise<Json> {
  switch (action.op) {
    case "submit":
      return submit(store, action);
    case "claim":
      return claim(store, time, leases, action);
    case "append":
    case "settle":
      return writeLease(store, time, leases, action);
    case "cancel":
      await runner.requestCancel(action.id);
      return { ok: true };
    case "resume":
      await runner.resume(action.id);
      return { ok: true };
    case "recover":
      return (await runner.recoverWithReport()).expired;
    case "checkpoint":
      return checkpoint(store, action.ids);
    case "import":
      return importConversation(store, action);
    case "readImport":
      return readImport(store, action);
  }
}

async function checkpoint(store: AssistantStore, ids: string[]): Promise<Json> {
  return json(
    await Promise.all(
      ids.map(async (id) => {
        const task = await store.tasks.getTask(id);
        if (!task) throw new Error(`Missing committed task ${id}`);
        const events = await store.tasks.listEvents(id);
        return {
          taskId: task.taskId,
          scopeId: task.scopeId,
          status: task.status,
          priority: task.priority,
          dependsOn: task.dependsOn ?? [],
          parentTaskId: task.parentTaskId ?? null,
          attemptCount: task.attemptCount,
          poisonCount: task.poisonCount,
          events: events.map((entry) => ({
            seq: entry.seq,
            id: entry.eventId,
          })),
          nextSeq: await store.tasks.nextSeq(id),
        };
      }),
    ),
  );
}
