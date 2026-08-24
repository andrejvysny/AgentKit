import { DuplicateTaskError } from "../errors.js";
import type { AssistantStore } from "../ports/assistant-store.js";
import type { Clock, IdGenerator } from "../ports/system.js";
import type { TaskRunner } from "../ports/task-runner.js";
import type { TaskRecord } from "../ports/task-store.js";

export interface TaskServiceDeps {
  store: AssistantStore;
  taskRunner: TaskRunner;
  ids: IdGenerator;
  /**
   * Not read by the methods below — the store stamps `enqueuedAt` and
   * `availableAt` from its OWN clock, and a second "now" here could disagree
   * with the one the row records. Taken so a host wires this service like every
   * other one, and so a caller computing a delayed `availableAt` has the same
   * clock the store will compare it against.
   */
  clock: Clock;
}

/** What a caller knows about a task before the store stamps the rest. */
export interface CreateTaskInputRequest {
  /** Omit to mint one. Supply it to make the submit idempotent on YOUR key. */
  taskId?: string;
  kind: string;
  scopeId: string;
  payload: Record<string, unknown>;
  priority?: number;
  availableAt?: string;
}

/** The subset of a task {@link TaskService.dispatch} needs to poke the queue. */
export interface DispatchTaskInput {
  taskId: string;
  scopeId: string;
  priority?: number;
  availableAt?: string;
}

/**
 * Creating a task and starting it, kept as two separable halves.
 *
 * They are separable because a host almost never wants only one of them: the
 * task row usually has to land in the SAME transaction as whatever domain rows
 * justify it (the messages of a turn, the document a reindex was triggered by),
 * and that transaction is the host's, not this service's. So
 * {@link TaskService.createTask} is a pure store write a host composes inside
 * its own `transaction`, {@link TaskService.dispatch} is the post-commit poke,
 * and {@link TaskService.submitTask} is the convenience that does both for the
 * simple case.
 */
export class TaskService {
  constructor(private readonly deps: TaskServiceDeps) {}

  /**
   * Write the `queued` task row through `tx`, and nothing else.
   *
   * NEVER enqueues — that is {@link TaskService.dispatch}'s job, and the split
   * is not stylistic (see the hazard on {@link TaskService.submitTask}). Pass
   * the host's own transaction view as `tx` to make the task atomic with the
   * rows it belongs to; pass the store itself for a standalone write.
   */
  async createTask(
    tx: AssistantStore,
    input: CreateTaskInputRequest,
  ): Promise<TaskRecord> {
    return tx.tasks.createTask({
      taskId: input.taskId ?? this.deps.ids.taskId(),
      kind: input.kind,
      scopeId: input.scopeId,
      payload: input.payload,
      ...(input.priority === undefined ? {} : { priority: input.priority }),
      ...(input.availableAt === undefined
        ? {}
        : { availableAt: input.availableAt }),
    });
  }

  /**
   * Tell the queue a persisted task is ready. Idempotent per `taskId` by the
   * port's own contract, so calling it twice for one task is a no-op, not a
   * second execution.
   */
  async dispatch(task: DispatchTaskInput): Promise<void> {
    await this.deps.taskRunner.enqueue({
      taskId: task.taskId,
      scopeId: task.scopeId,
      ...(task.priority === undefined ? {} : { priority: task.priority }),
      ...(task.availableAt === undefined
        ? {}
        : { availableAt: task.availableAt }),
    });
  }

  /**
   * Create a task and hand it to the queue.
   *
   * THE DISPATCH HAPPENS STRICTLY AFTER THE TRANSACTION RESOLVES, and that
   * ordering is load-bearing rather than tidy. Over `bun:sqlite` a store call
   * issued while a transaction callback is awaiting other async work JOINS that
   * open transaction (see the isolation caveat on
   * {@link AssistantStore.transaction}). Enqueue inside the callback and the
   * claim loop it wakes runs its own store calls inside our transaction: it can
   * claim the very row we are still writing, start executing it — and then a
   * rollback deletes the task out from under a running worker. Awaiting the
   * commit first makes the row real before anyone is told about it.
   *
   * Idempotent resubmit: when the caller supplied a `taskId` that already
   * exists, the duplicate is not an error but a redelivery — the existing task
   * is re-dispatched (a no-op if it is already running or finished) and
   * returned. A caller reusing an id for a DIFFERENT kind is a bug, not a
   * redelivery, so that rethrows.
   */
  async submitTask(input: CreateTaskInputRequest): Promise<TaskRecord> {
    const taskId = input.taskId ?? this.deps.ids.taskId();
    let task: TaskRecord;
    try {
      task = await this.deps.store.transaction((tx) =>
        this.createTask(tx, { ...input, taskId }),
      );
    } catch (err) {
      const existing = await this.onDuplicate(err, input, taskId);
      if (existing === null) throw err;
      task = existing;
    }
    await this.dispatch(task);
    return task;
  }

  /**
   * The existing task when `err` is a duplicate of a caller-supplied id for the
   * same kind; null when the caller should see the original throw.
   */
  private async onDuplicate(
    err: unknown,
    input: CreateTaskInputRequest,
    taskId: string,
  ): Promise<TaskRecord | null> {
    // A minted id that collides is not a redelivery — it is a broken
    // IdGenerator, and swallowing it would hand the caller someone else's task.
    if (!(err instanceof DuplicateTaskError) || input.taskId === undefined) {
      return null;
    }
    const existing = await this.deps.store.tasks.getTask(taskId);
    if (!existing || existing.kind !== input.kind) return null;
    return existing;
  }
}
