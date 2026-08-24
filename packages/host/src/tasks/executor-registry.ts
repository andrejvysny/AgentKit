import {
  AgentKitHostError,
  ExecutorNotFoundError,
  RecordNotFoundError,
} from "../errors.js";
import type { AssistantStore } from "../ports/assistant-store.js";
import { defaultClock, type Clock, type Logger } from "../ports/system.js";
import type { TaskExecution, TaskWorker } from "../ports/task-runner.js";
import type { TaskExecutor } from "./task-executor.js";

/**
 * The kind → executor table one worker process dispatches through.
 *
 * A registry rather than a switch because which kinds a process can run is a
 * DEPLOYMENT decision: the same build serves a box that only answers chat turns
 * and a box that only reindexes. {@link ExecutorRegistry.kinds} is what such a
 * deployment feeds to `ClaimNextInput.kinds` so neither box claims the other's
 * work. (The reference `SingleProcessTaskRunner` does not: `StartWorkerOptions`
 * has no kinds field, so it claims every kind and relies on every executor
 * being registered in the one process — an unregistered kind then fails with
 * {@link ExecutorNotFoundError} rather than waiting for a worker that could
 * have run it.)
 */
export class ExecutorRegistry {
  private readonly executors = new Map<string, TaskExecutor>();

  /**
   * Register one executor.
   *
   * A duplicate kind throws rather than replacing: two executors for one kind
   * means whichever module was imported last silently wins, and the resulting
   * "my executor never runs" is invisible until production. Registration is a
   * boot-time act, so failing loudly there costs nothing.
   */
  register(executor: TaskExecutor): void {
    if (this.executors.has(executor.kind)) {
      throw new AgentKitHostError(
        "duplicate_executor_kind",
        `An executor is already registered for task kind "${executor.kind}".`,
        { kind: executor.kind },
      );
    }
    this.executors.set(executor.kind, executor);
  }

  get(kind: string): TaskExecutor | undefined {
    return this.executors.get(kind);
  }

  /** Every registered kind, in registration order — a kind-scoped worker's
   *  `ClaimNextInput.kinds`. */
  kinds(): string[] {
    return [...this.executors.keys()];
  }
}

export interface DispatchingWorkerDeps {
  store: AssistantStore;
  /**
   * Stamps `startedAt` on the direct-execute `queued → running` transition
   * below. Defaults to {@link defaultClock}; injected in tests so the timestamp
   * is a statement about a fake clock rather than about wall-clock.
   */
  clock?: Clock;
  logger?: Logger;
}

/**
 * The {@link TaskWorker} a host hands to `TaskRunner.startWorker`: load the
 * task, guard it, and route it to the executor for its kind.
 *
 * The load-and-guard lives here, once, rather than in each executor — see
 * {@link TaskExecutionContext} on why the record is passed down instead of the
 * id. The `queued → running` branch is for the direct-execute path (a host or a
 * test that calls `execute` without going through a claim); `claimNext` already
 * performs that transition atomically as part of claiming, so on the normal
 * queue path the task arrives here already `running` and nothing is written.
 */
export function createDispatchingWorker(
  registry: ExecutorRegistry,
  deps: DispatchingWorkerDeps,
): TaskWorker {
  const clock = deps.clock ?? defaultClock;
  return {
    async execute(execution: TaskExecution): Promise<void> {
      const { taskId, attemptId, leaseToken, signal } = execution;
      const tasks = deps.store.tasks;
      const loaded = await tasks.getTask(taskId);
      if (!loaded) {
        throw new RecordNotFoundError(`Task not found: ${taskId}`, { taskId });
      }
      if (loaded.status !== "queued" && loaded.status !== "running") {
        throw new AgentKitHostError(
          "task_not_executable",
          `Task ${taskId} is ${loaded.status}; only queued or running tasks execute.`,
          { taskId, status: loaded.status },
        );
      }
      const task =
        loaded.status === "queued"
          ? await tasks.transitionTask(taskId, ["queued"], "running", {
              startedAt: clock.nowIso(),
            })
          : loaded;

      const executor = registry.get(task.kind);
      if (!executor) {
        // NOT a dead-letter: the dead-letter row means "this poisoned the
        // queue". A kind nobody registered is a wiring mistake with a clean
        // diagnosis, so the classifier calls it terminal and the runner fails
        // the task — see `settleThrown`, which splits those two outcomes.
        throw new ExecutorNotFoundError(
          `No executor registered for task kind "${task.kind}" (task ${taskId}).`,
          { taskId, kind: task.kind },
        );
      }
      deps.logger?.debug("dispatching task to its executor", {
        taskId,
        kind: task.kind,
        attemptId,
      });
      await executor.execute({ task, attemptId, leaseToken, signal });
    },
  };
}
