import type { AssistantStore, AttemptRecord } from "@agentkit/host";
import { Database } from "bun:sqlite";

/** Observe actual adapter returns without replacing its state transitions. */
export function observeAttempts(
  store: AssistantStore,
  path: string,
): () => string[] {
  const observed = new Map<string, AttemptRecord>();
  const tasks = store.tasks;
  const claim = tasks.claimNext.bind(tasks);
  const create = tasks.createAttempt.bind(tasks);
  const end = tasks.endAttempt.bind(tasks);
  tasks.claimNext = async (input) => {
    const result = await claim(input);
    if (result) observed.set(result.attempt.attemptId, result.attempt);
    return result;
  };
  tasks.createAttempt = async (input) => {
    const result = await create(input);
    observed.set(result.attemptId, result);
    return result;
  };
  tasks.endAttempt = async (input) => {
    const result = await end(input);
    observed.set(result.attemptId, result);
    return result;
  };
  return () =>
    path === ":memory:"
      ? [...observed.values()]
          .sort((a, b) => a.attemptNumber - b.attemptNumber)
          .map((entry) => entry.status)
      : persistedAttempts(path);
}

function persistedAttempts(path: string): string[] {
  const db = new Database(path, { readonly: true });
  try {
    return (
      db
        .query(
          "SELECT status FROM task_attempts WHERE task_id='run' ORDER BY attempt_number",
        )
        .all() as { status: string }[]
    ).map((row) => row.status);
  } finally {
    db.close();
  }
}
