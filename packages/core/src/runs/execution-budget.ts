export interface ExecutionBudgets {
  firstByteMs?: number;
  streamIdleMs?: number;
  toolMs?: number;
  overallMs?: number;
  providerRequests?: number;
  toolCalls?: number;
  correctionPasses?: number;
  retries?: number;
}

export interface ExecutionBudgetState {
  version: 1;
  startedAt: number;
  providerRequests: number;
  toolCalls: number;
  correctionPasses: number;
  retries: number;
}
export type ExecutionCounter =
  | "providerRequests"
  | "toolCalls"
  | "correctionPasses"
  | "retries";

export class ExecutionBudgetError extends Error {
  constructor(
    readonly code: "execution_timeout" | "execution_budget_exhausted",
    readonly phase: string,
  ) {
    super(`${code}: ${phase}`);
    this.name = "ExecutionBudgetError";
  }
}

function validateLimits(limits: Readonly<ExecutionBudgets>): void {
  for (const [key, value] of Object.entries(limits)) {
    if (!Number.isSafeInteger(value) || value < 0)
      throw new RangeError(`Invalid execution budget: ${key}`);
  }
}

function restoreState(
  previous: ExecutionBudgetState | undefined,
  now: () => number,
): ExecutionBudgetState {
  const state = previous
    ? { ...previous }
    : {
        version: 1 as const,
        startedAt: now(),
        providerRequests: 0,
        toolCalls: 0,
        correctionPasses: 0,
        retries: 0,
      };
  if (
    state.version !== 1 ||
    [
      state.startedAt,
      state.providerRequests,
      state.toolCalls,
      state.correctionPasses,
      state.retries,
    ].some((value) => !Number.isSafeInteger(value) || value < 0)
  )
    throw new RangeError("Invalid execution budget state");
  return state;
}

/** One instance spans every provider/correction pass; snapshot before external work. */
export class ExecutionBudget {
  readonly signal: AbortSignal;
  private readonly controller = new AbortController();
  private readonly state: ExecutionBudgetState;
  private readonly parent?: AbortSignal;
  private readonly onAbort: () => void;
  private timer?: ReturnType<typeof setTimeout>;

  constructor(
    readonly limits: Readonly<ExecutionBudgets> = {},
    options: {
      signal?: AbortSignal;
      state?: ExecutionBudgetState;
      now?: () => number;
      checkpoint?: (state: ExecutionBudgetState) => Promise<void>;
    } = {},
  ) {
    this.limits = Object.freeze({ ...limits });
    this.now = options.now ?? Date.now;
    this.checkpoint = options.checkpoint;
    validateLimits(limits);
    this.state = restoreState(options.state, this.now);
    this.parent = options.signal;
    this.onAbort = () => this.controller.abort(this.parent?.reason);
    if (this.parent?.aborted) this.onAbort();
    else this.parent?.addEventListener("abort", this.onAbort, { once: true });
    this.signal = this.controller.signal;
    if (limits.overallMs !== undefined) {
      const remaining = this.state.startedAt + limits.overallMs - this.now();
      if (remaining <= 0) this.timeout("overall");
      else this.timer = setTimeout(() => this.timeout("overall"), remaining);
    }
  }
  private readonly now: () => number;
  private readonly checkpoint?: (state: ExecutionBudgetState) => Promise<void>;

  snapshot(): ExecutionBudgetState {
    return { ...this.state };
  }
  check(): void {
    this.signal.throwIfAborted();
  }
  timeout(phase: string): void {
    this.controller.abort(new ExecutionBudgetError("execution_timeout", phase));
  }

  async consume(counter: ExecutionCounter): Promise<void> {
    this.check();
    const limit = this.limits[counter];
    if (limit !== undefined && this.state[counter] >= limit) {
      const error = new ExecutionBudgetError(
        "execution_budget_exhausted",
        counter,
      );
      this.controller.abort(error);
      throw error;
    }
    this.state[counter]++;
    await this.checkpoint?.(this.snapshot());
    this.check();
  }

  async race<T>(
    work: () => PromiseLike<T>,
    timeoutMs?: number,
    phase = "execution",
  ): Promise<T> {
    this.check();
    let timer: ReturnType<typeof setTimeout> | undefined;
    let abort: () => void = () => {};
    const cancelled = new Promise<never>((_resolve, reject) => {
      abort = () => reject(this.signal.reason);
      this.signal.addEventListener("abort", abort, { once: true });
      if (timeoutMs !== undefined)
        timer = setTimeout(() => this.timeout(phase), timeoutMs);
    });
    try {
      const result = await Promise.race([
        Promise.resolve().then(() => {
          this.check();
          return work();
        }),
        cancelled,
      ]);
      this.check();
      return result;
    } finally {
      if (timer !== undefined) clearTimeout(timer);
      this.signal.removeEventListener("abort", abort);
    }
  }

  dispose(): void {
    if (this.timer !== undefined) clearTimeout(this.timer);
    this.parent?.removeEventListener("abort", this.onAbort);
  }
}
