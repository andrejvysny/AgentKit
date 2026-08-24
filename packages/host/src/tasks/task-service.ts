import { DuplicateTaskError, InvalidTaskTransitionError } from "../errors.js";
import type { AssistantStore } from "../ports/assistant-store.js";
import type { Clock, IdGenerator } from "../ports/system.js";
import type { TaskRunner } from "../ports/task-runner.js";
import type { TaskRecord } from "../ports/task-store.js";

export interface TaskServiceDeps {
  store: AssistantStore;
  taskRunner: TaskRunner;
  ids: IdGenerator;
  /**
   * Read only by {@link TaskService.cancelTask}, for the `finishedAt` it stamps
   * on a cancelled row — the create path does NOT use it, because the store
   * stamps `enqueuedAt` and `availableAt` from its OWN clock and a second "now"
   * here could disagree with the one the row records. Taken so a host wires
   * this service like every other one, and so a caller computing a delayed
   * `availableAt` has the same clock the store will compare it against.
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
  /**
   * Lineage — see {@link TaskRecord.parentTaskId}. Usually not set by hand:
   * `TaskExecutionContext.spawnChild` presets it to the spawning task.
   */
  parentTaskId?: string;
  /**
   * Ids that must complete before this task is claimable — see
   * {@link TaskRecord.dependsOn}. Every one of them must already be persisted,
   * so a continuation is submitted AFTER the tasks it waits on.
   */
  dependsOn?: string[];
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
      ...(input.parentTaskId === undefined
        ? {}
        : { parentTaskId: input.parentTaskId }),
      ...(input.dependsOn === undefined ? {} : { dependsOn: input.dependsOn }),
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
   * returned. A caller reusing an id for a DIFFERENT kind or a DIFFERENT scope
   * is a bug, not a redelivery, so that rethrows.
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
   * Cancel a task and, breadth-first, every task descended from it.
   *
   * The cascade follows `parentTaskId`, not `dependsOn`: children are work this
   * task set off, so abandoning the parent abandons the branch. Tasks merely
   * WAITING on the cancelled one are not touched here — they settle themselves
   * on the next claim, because `claimNext` reads a cancelled dependency as
   * "cancel the dependent" (see {@link evaluateTaskDependencies}). One cascade
   * per edge type, each enforced where that edge lives.
   *
   * WHAT IT DOES NOT DO IS FORCE A RUNNING TASK TERMINAL. Flipping a row to
   * `cancelled` under a worker that is still executing would produce a task the
   * store calls finished while its executor keeps writing events and side
   * effects — two answers to "did this run?". So a running (or approval-parked)
   * descendant is asked to stop through the queue's own cancel path, which
   * aborts the execution signal and lets the worker land the task itself.
   * Cancellation of running work is cooperative by construction; this method
   * makes the request, the executor honours it.
   *
   * Cancelling something already terminal is a no-op, and so is cancelling a
   * task that finished between the read and the write — that lost CAS race
   * means the task landed on its own outcome, which is the truer one.
   */
  async cancelTask(taskId: string): Promise<void> {
    const visited = new Set<string>();
    const frontier: string[] = [taskId];
    while (frontier.length > 0) {
      const current = frontier.shift()!;
      if (visited.has(current)) continue;
      visited.add(current);
      await this.cancelOne(current);
      // Descend even from a task that was already terminal: a completed parent
      // can still have queued children waiting to run.
      for (const child of await this.deps.store.tasks.listChildren(current)) {
        frontier.push(child.taskId);
      }
    }
  }

  /** One node of the {@link TaskService.cancelTask} walk. */
  private async cancelOne(taskId: string): Promise<void> {
    const tasks = this.deps.store.tasks;
    const task = await tasks.getTask(taskId);
    if (!task) return;
    if (task.status === "running" || task.status === "waiting_approval") {
      await this.deps.taskRunner.requestCancel(taskId);
      return;
    }
    if (task.status !== "queued") return;
    // A queued task is cancelled in the STORE, not through the runner: nobody
    // has claimed it, so there is no execution to stop, and the row is the only
    // thing that decides whether it ever starts.
    try {
      await tasks.transitionTask(taskId, ["queued"], "cancelled", {
        finishedAt: this.deps.clock.nowIso(),
      });
    } catch (err) {
      // A claim or another cancel won the race between the read above and this
      // write. Anything else is a real store failure and must not be swallowed.
      if (!(err instanceof InvalidTaskTransitionError)) throw err;
    }
  }

  /**
   * The existing task when `err` is a duplicate of a caller-supplied id for the
   * same work; null when the caller should see the original throw.
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
    // Kind AND scope: the scope is what the queue serializes on, so a task
    // under this id that runs against a different one is not the work this
    // caller asked for — it is two callers colliding on a key. Returning it
    // would report someone else's task as this caller's, and re-dispatch it
    // into the bargain.
    if (
      !existing ||
      existing.kind !== input.kind ||
      existing.scopeId !== input.scopeId
    ) {
      return null;
    }
    return existing;
  }
}
