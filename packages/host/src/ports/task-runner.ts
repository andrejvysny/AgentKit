/**
 * The durable queue that turns "a task exists" into "a worker is executing it".
 *
 * Note what is NOT here: `subscribe()`. Events reach consumers through the task
 * event log and the outbox, both of which survive a restart. A subscription on
 * the runner would be an in-memory second channel that silently delivers less
 * than the durable one after any crash — two sources of truth, one of them
 * lossy.
 */

import { ExecutionInterruptedError } from "../errors.js";

/** A host interruption parks work until explicit resume; it is not user cancellation. */
export interface TaskInterruptionReason {
  code: string;
  message: string;
}

/** The reason {@link TaskRunner.requestInterrupt} records when its caller names none. */
export const DEFAULT_INTERRUPTION_REASON: TaskInterruptionReason =
  Object.freeze({
    code: "INTERRUPTED",
    message: "execution interrupted",
  });

/**
 * A resumable interrupt that carries its reason. It IS an
 * {@link ExecutionInterruptedError}, so a worker that parks on the base class
 * also parks on this one; hosts that need the reason test for the subclass.
 */
export class TaskInterruptedError extends ExecutionInterruptedError {
  readonly code: string;
  constructor(readonly reason: TaskInterruptionReason) {
    super(reason.message);
    this.name = "TaskInterruptedError";
    this.code = reason.code;
  }
}

/** What a worker is handed for one attempt at one task. */
export interface TaskExecution {
  taskId: string;
  attemptId: string;
  /** Proof of ownership; every store write for this attempt must carry it. */
  leaseToken: string;
  /** Aborted on cancellation, and when the lease can no longer be renewed. */
  signal: AbortSignal;
}

export interface TaskWorker {
  execute(execution: TaskExecution): Promise<void>;
}

export interface EnqueueInput {
  taskId: string;
  /** Serialization key: two tasks in one scope never execute concurrently. */
  scopeId: string;
  priority?: number;
  /** Delay the first claim until this instant. */
  availableAt?: string;
}

export interface StartWorkerOptions {
  concurrency?: number;
  ownerId?: string;
  /**
   * Restrict this worker to these task kinds, forwarded verbatim as
   * {@link ClaimNextInput.kinds}.
   *
   * The store has always been able to filter a claim by kind; without this
   * option nothing could ask it to, so the documented multi-pool deployment (a
   * GPU box registering `index.embed`, a web box registering `chat.turn`) was
   * unreachable — every worker claimed everything and failed on
   * `ExecutorNotFoundError` for work another process was there to run. Absent
   * means "any kind"; an EMPTY array means "no kind", which is what a worker
   * with an empty executor registry actually wants.
   */
  kinds?: string[];
}

export interface WorkerHandle {
  /** Stop claiming and wait for in-flight attempts to settle. */
  stop(): Promise<void>;
}

export interface TaskRunner {
  /**
   * Idempotent per `taskId`: re-delivering the same enqueue (a retried HTTP
   * call, a replayed message) must not create a second execution of one task.
   */
  enqueue(input: EnqueueInput): Promise<void>;

  /**
   * Ask the task to stop. Queued tasks are cancelled outright; a running one has
   * its execution signal aborted, and the worker decides how to land.
   */
  requestCancel(taskId: string): Promise<void>;

  /**
   * Ask the task to stop RESUMABLY: queued → `interrupted` outright; a running
   * one has its signal aborted with {@link TaskInterruptedError} and is parked
   * `interrupted` (not terminal, no terminal event, attempt `cancelled`).
   * `resume` continues the same task with a new attempt. `reason` defaults to
   * {@link DEFAULT_INTERRUPTION_REASON} and is stored as the task's `error`.
   */
  requestInterrupt?(
    taskId: string,
    reason?: TaskInterruptionReason,
  ): Promise<void>;

  /**
   * Startup pass: expire dead leases, end their attempts as `abandoned`,
   * reconcile interrupted applies by operation id, then re-enqueue or
   * dead-letter. Runs before any worker starts claiming.
   */
  recover(): Promise<void>;

  /** Explicitly resume an interrupted run using its original identity. */
  resume?(taskId: string): Promise<void>;

  startWorker(
    worker: TaskWorker,
    opts?: StartWorkerOptions,
  ): Promise<WorkerHandle>;
}
