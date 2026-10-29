import type { ExecutionCleanup } from "./coordination.js";
import { CodingError } from "./errors.js";

/** Commands remain owned here even if their caller races cancellation. */
export class CommandLifecycle {
  private readonly active = new Map<
    symbol,
    { controller: AbortController; task: Promise<unknown> }
  >();
  private readonly cleanup = new Map<string, ExecutionCleanup>();
  private closing = false;

  assertOpen(): void {
    if (this.closing)
      throw new CodingError("coding_disposed", "coding tool pack is closing");
  }

  record(result: ExecutionCleanup): void {
    this.cleanup.set(result.executionAttemptId, result);
  }

  run<T>(
    signal: AbortSignal | undefined,
    work: (signal: AbortSignal) => Promise<T>,
  ): Promise<T> {
    this.assertOpen();
    const id = Symbol();
    const controller = new AbortController();
    const combined = signal
      ? AbortSignal.any([controller.signal, signal])
      : controller.signal;
    const task = Promise.resolve().then(() => work(combined));
    this.active.set(id, { controller, task });
    return task.finally(() => this.active.delete(id));
  }

  async drain(): Promise<ExecutionCleanup[]> {
    this.closing = true;
    const active = [...this.active.values()];
    for (const { controller } of active) controller.abort();
    await Promise.allSettled(active.map(({ task }) => task));
    return [...this.cleanup.values()];
  }
}
