import type { TaskRecord } from "../ports/task-store.js";

/**
 * What one executor is handed for one attempt at one task.
 *
 * The already-loaded {@link TaskRecord} is the point. The dispatcher has just
 * fetched the row and checked that it is executable; handing the executor the
 * id instead would make every executor re-fetch and re-validate, and the second
 * read can legitimately disagree with the first (a cancel landed in between) —
 * so two executors would each invent their own answer to "is this still mine to
 * run?". One read, one verdict, passed down.
 *
 * The rest mirrors {@link TaskExecution}: `attemptId` and `leaseToken` are what
 * every durable write must carry, and `signal` aborts on cancellation and on a
 * lease that can no longer be renewed.
 */
export interface TaskExecutionContext {
  task: TaskRecord;
  attemptId: string;
  leaseToken: string;
  signal: AbortSignal;
}

/**
 * The unit of work behind one task kind.
 *
 * An executor owns its own terminal bookkeeping the way `TurnRunner` does —
 * transition the task, end the attempt — or it returns and lets the task runner
 * settle it. Throwing is how it reports failure: the runner classifies the error
 * (see `classifyExecutionError`) and decides retry vs terminal vs dead-letter,
 * which is a queue decision, not an executor one.
 */
export interface TaskExecutor {
  /** The `TaskRecord.kind` this executor claims. Unique per registry. */
  readonly kind: string;
  execute(ctx: TaskExecutionContext): Promise<void>;
}
