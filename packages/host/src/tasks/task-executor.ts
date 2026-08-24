import type { TaskRecord } from "../ports/task-store.js";
import type { CreateTaskInputRequest } from "./task-service.js";

/**
 * What an executor may ask for when it fans work out.
 *
 * Everything a normal submit takes EXCEPT `parentTaskId`: the dispatcher
 * presets that to the spawning task's id, so lineage cannot be forged or
 * forgotten by the executor. `dependsOn` stays available — a child that must
 * wait on an earlier sibling says so — and so does the whole continuation
 * pattern: spawn two children, then spawn a third task that `dependsOn` both.
 */
export type SpawnChildInput = Omit<CreateTaskInputRequest, "parentTaskId">;

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
  /**
   * Submit a child task, with `parentTaskId` preset to the task being executed.
   *
   * PRESENT ONLY when the dispatcher was given a `TaskService` — spawning is a
   * submit, and a submit is create-then-dispatch against a queue, which is
   * exactly the dependency a bare executor registry does not have. An executor
   * that needs to fan out therefore checks for it (`ctx.spawnChild?.(...)`, or
   * throws its own diagnosis) rather than a worker silently dropping the
   * children on the floor.
   *
   * The child is an ordinary task: durable, claimed by whichever worker is
   * free, with its own attempts, lease and event log. It is NOT awaited by the
   * parent and does not keep it alive — a parent that must see its children
   * finish spawns a continuation task that `dependsOn` them.
   */
  spawnChild?(input: SpawnChildInput): Promise<TaskRecord>;
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
