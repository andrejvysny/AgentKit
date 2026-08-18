/**
 * In-memory, per-scope serialization for {@link SingleProcessTaskRunner}.
 *
 * A scope is the key two runs must not execute under at the same time (usually
 * a chat id — two turns interleaving into one message history is the bug this
 * prevents). One run per scope is "active"; the rest wait in FIFO order.
 *
 * WHAT THIS IS NOT: the durable truth. That lives in the store — `claimNext`
 * only ever selects `queued` runs and hands back a lease, and the lease is what
 * makes ownership survive a crash. This lock is a DISPATCH OPTIMIZATION: it
 * feeds `claimNext`'s `scopesBusy` so the claim skips scopes this process is
 * already working, and it answers "where am I in line?" for a UI. Correctness
 * must never depend on it alone — wipe the whole thing between two claims and
 * the store still refuses to hand the same run to two workers.
 *
 * Salvaged from task-system's `ScopeTaskLock`/`ChatTaskLock`, with three
 * changes:
 *
 * 1. `getPosition` / `hasActive` are back (they existed in the original
 *    `ChatTaskLock` and were dropped in the rewrite) — a queue whose position
 *    nobody can read is a queue a user cannot be told about.
 * 2. `tryAcquire` is RE-ENTRANT for the run that already holds the scope. The
 *    original pushed the active run into its own wait list, so a recovery pass
 *    re-dispatching a still-active run silently made it wait behind itself.
 * 3. `release` does NOT promote the next waiter to active. In this runner the
 *    store's `claimNext` is what actually starts work, and a scope marked busy
 *    for a run nobody has claimed yet would be skipped by every future claim —
 *    a self-inflicted deadlock. `release` pops the successor and leaves the
 *    scope FREE; promotion happens when the dispatch loop claims that run and
 *    calls `tryAcquire`.
 */
export class ScopeLock {
  /** scopeId → the runId currently executing in it. */
  private readonly activeByScope = new Map<string, string>();
  /** scopeId → runIds waiting, oldest first. */
  private readonly waitingByScope = new Map<string, string[]>();

  /**
   * Take the scope for `runId`, or record it as a waiter.
   *
   * Returns true when the caller may execute (the scope was free, or `runId`
   * already held it). Returns false when another run holds the scope; `runId`
   * is appended to the FIFO exactly once, so a repeated call does not double it.
   */
  tryAcquire(scopeId: string, runId: string): boolean {
    const active = this.activeByScope.get(scopeId);
    if (active === undefined || active === runId) {
      this.activeByScope.set(scopeId, runId);
      // Becoming active means leaving the line — otherwise a later release
      // would hand this same run back as its own successor.
      this.dropWaiter(scopeId, runId);
      return true;
    }
    const waiting = this.waitingByScope.get(scopeId) ?? [];
    if (!waiting.includes(runId)) {
      waiting.push(runId);
      this.waitingByScope.set(scopeId, waiting);
    }
    return false;
  }

  /**
   * Give up the scope and pop the next waiter, which the CALLER dispatches
   * (here: by kicking the claim loop). The scope is left free — see the class
   * doc on why the successor is not promoted to active.
   *
   * A caller that does not hold the scope only loses its place in line: that is
   * the cancelled-while-waiting case, not a release.
   */
  release(scopeId: string, runId: string): string | null {
    if (this.activeByScope.get(scopeId) !== runId) {
      this.dropWaiter(scopeId, runId);
      return null;
    }
    this.activeByScope.delete(scopeId);
    const waiting = this.waitingByScope.get(scopeId);
    if (!waiting || waiting.length === 0) return null;
    const next = waiting.shift() ?? null;
    if (waiting.length === 0) this.waitingByScope.delete(scopeId);
    return next;
  }

  /** Scopes with a run executing — exactly what `claimNext` must skip. */
  busyScopes(): string[] {
    return [...this.activeByScope.keys()];
  }

  hasActive(scopeId: string): boolean {
    return this.activeByScope.has(scopeId);
  }

  /** The run executing in `scopeId`, if any. */
  activeRun(scopeId: string): string | null {
    return this.activeByScope.get(scopeId) ?? null;
  }

  /** 0 when active, 1-based position when waiting, -1 when unknown here. */
  getPosition(scopeId: string, runId: string): number {
    if (this.activeByScope.get(scopeId) === runId) return 0;
    const index = this.waitingByScope.get(scopeId)?.indexOf(runId) ?? -1;
    return index < 0 ? -1 : index + 1;
  }

  /**
   * Forget `runId` entirely — the cancellation path. Clears the active slot if
   * it held it (WITHOUT promoting a successor: the caller kicks the loop and
   * the store decides who runs next) and drops it from the FIFO otherwise.
   */
  remove(scopeId: string, runId: string): void {
    if (this.activeByScope.get(scopeId) === runId) {
      this.activeByScope.delete(scopeId);
    }
    this.dropWaiter(scopeId, runId);
  }

  /** Waiting runs for a scope, oldest first. Observability only. */
  waiting(scopeId: string): string[] {
    return [...(this.waitingByScope.get(scopeId) ?? [])];
  }

  /** Drop all state (shutdown / tests). */
  clear(): void {
    this.activeByScope.clear();
    this.waitingByScope.clear();
  }

  private dropWaiter(scopeId: string, runId: string): void {
    const waiting = this.waitingByScope.get(scopeId);
    if (!waiting) return;
    const index = waiting.indexOf(runId);
    if (index < 0) return;
    waiting.splice(index, 1);
    if (waiting.length === 0) this.waitingByScope.delete(scopeId);
  }
}
