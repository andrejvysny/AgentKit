/**
 * The durable queue that turns "a task exists" into "a worker is executing it".
 *
 * Note what is NOT here: `subscribe()`. Events reach consumers through the task
 * event log and the outbox, both of which survive a restart. A subscription on
 * the runner would be an in-memory second channel that silently delivers less
 * than the durable one after any crash — two sources of truth, one of them
 * lossy.
 */

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
   * Startup pass: expire dead leases, end their attempts as `abandoned`,
   * reconcile interrupted applies by operation id, then re-enqueue or
   * dead-letter. Runs before any worker starts claiming.
   */
  recover(): Promise<void>;

  startWorker(
    worker: TaskWorker,
    opts?: StartWorkerOptions,
  ): Promise<WorkerHandle>;
}
