import { AgentKitHostError, RecordNotFoundError } from "../errors.js";
import type { AssistantStore } from "../ports/assistant-store.js";
import type { Clock } from "../ports/system.js";
import type { TaskRecord } from "../ports/task-store.js";

/**
 * Load a task and prove it is still runnable, returning the record an executor
 * should work on.
 *
 * The guard exists so exactly one of "this task is executable" and "somebody
 * already moved it" is true before the model — or any other side effect — is
 * touched. Anything already `completed`, `cancelled` or `dead_letter` is a
 * task somebody else finished; running it again would append a second answer to
 * a conversation that already has one.
 *
 * The `queued → running` branch is for the DIRECT-EXECUTE path (a host or a
 * test that calls a worker without going through a claim). `claimNext` already
 * performs that transition atomically as part of claiming, so on the normal
 * queue path the task arrives here already `running` and nothing is written.
 *
 * Lives in its own module because two entry points need it —
 * `createDispatchingWorker` and `TurnRunner.execute` — and their guards drifting
 * apart is precisely the bug that would let one of them start a finished task.
 */
export async function loadExecutableTask(
  store: AssistantStore,
  taskId: string,
  clock: Clock,
): Promise<TaskRecord> {
  const tasks = store.tasks;
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
  if (loaded.status === "running") return loaded;
  return tasks.transitionTask(taskId, ["queued"], "running", {
    startedAt: clock.nowIso(),
  });
}
