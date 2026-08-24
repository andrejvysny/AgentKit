/**
 * Priority aging for the reference adapters' `claimNext` — the starvation
 * valve, and OFF unless a host asks for it.
 *
 * THE FORMULA IS PRESERVED FROM task-system, the predecessor runtime this
 * workspace salvages ideas from: a task's effective priority is its base
 * priority plus one bonus step per aging interval it has waited,
 *
 *   effective = priority + min(maxBonus, floor(waitMs / intervalMs) * bonus)
 *
 * with `waitMs` measured from `enqueuedAt` to the `now` the CALLER passed to
 * `claimNext`. What changed is the default and the ceiling. task-system aged
 * every queue unconditionally and without a cap, which quietly makes `priority`
 * mean "head start" rather than "priority": wait long enough and a background
 * sweep outranks an interactive turn, on a queue whose owner never asked for
 * that trade. So `agingBonus` defaults to 0 — the ordering is exactly
 * `priority DESC, enqueuedAt ASC` until a host opts in — and `agingMaxBonus`
 * bounds how far a waiting task may climb once it does, so an aged task can be
 * allowed to overtake its peers without ever overtaking a whole priority class.
 *
 * Measured against the caller's `now` rather than the store's own clock for the
 * reason `ClaimNextInput.now` exists at all: the value that filters
 * `availableAt` and the value that ages a row must be the same instant, and a
 * test that cannot move it cannot assert any of this.
 */

/** Aging knobs an adapter accepts at construction. Absent ⇒ aging is off. */
export interface TaskAgingOptions {
  /** How long a task must wait to earn one `agingBonus` step. Default 30s. */
  agingIntervalMs?: number;
  /** Priority added per elapsed interval. Default 0 — aging disabled. */
  agingBonus?: number;
  /** Ceiling on the total bonus, however long the wait. Default: uncapped. */
  agingMaxBonus?: number;
}

/** {@link TaskAgingOptions} with every default filled in. */
export interface ResolvedTaskAging {
  intervalMs: number;
  bonus: number;
  maxBonus: number;
}

const DEFAULT_AGING_INTERVAL_MS = 30_000;

/**
 * Fill in the defaults, and refuse an interval of 0 (a division by zero that
 * would silently produce `Infinity` priorities rather than an error anyone
 * could debug).
 */
export function resolveTaskAging(
  options: TaskAgingOptions = {},
): ResolvedTaskAging {
  const intervalMs = options.agingIntervalMs ?? DEFAULT_AGING_INTERVAL_MS;
  return {
    intervalMs: intervalMs > 0 ? intervalMs : DEFAULT_AGING_INTERVAL_MS,
    bonus: options.agingBonus ?? 0,
    maxBonus: options.agingMaxBonus ?? Number.MAX_SAFE_INTEGER,
  };
}

/** The formula in the module doc, for the in-memory comparator. */
export function effectivePriority(
  priority: number,
  enqueuedAtMs: number,
  nowMs: number,
  aging: ResolvedTaskAging,
): number {
  if (aging.bonus === 0) return priority;
  const waitMs = Math.max(0, nowMs - enqueuedAtMs);
  const earned = Math.floor(waitMs / aging.intervalMs) * aging.bonus;
  return priority + Math.min(aging.maxBonus, earned);
}
