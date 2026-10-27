/**
 * Opt-in, bounded retry of one model request after a transient provider
 * failure. Only applied while nothing from the failed request was shown or
 * acted on; never switches provider.
 */
export interface ProviderRetryPolicy {
  /** Total requests including the first; valid range 1..6 (1 = no retry). */
  maxAttempts: number;
  /** Delay before the first retry; doubles per attempt. */
  baseDelayMs: number;
  /** Upper bound of the pre-jitter delay. */
  maxDelayMs: number;
  /** Test seam returning [0, 1). Default `Math.random`. */
  random?: () => number;
  /** Test seam. Default resolves early when `signal` aborts. */
  sleep?: (ms: number, signal?: AbortSignal) => Promise<void>;
}

const MAX_ATTEMPTS_CEILING = 6;

const RETRYABLE_CODES: ReadonlySet<string> = new Set([
  "network_error",
  "empty_body",
  "incomplete_stream",
  "provider_error",
  "408",
  "425",
  "429",
  "500",
  "502",
  "503",
  "504",
  "529",
  "service_unavailable",
  "usage_limit",
]);

export function isRetryableProviderCode(code: string | undefined): boolean {
  return code !== undefined && RETRYABLE_CODES.has(code);
}

/** The usable max attempts, or 1 (no retry) for an absent or invalid policy. */
export function effectiveMaxAttempts(
  policy: ProviderRetryPolicy | undefined,
): number {
  if (policy === undefined) return 1;
  const { maxAttempts, baseDelayMs, maxDelayMs } = policy;
  if (
    !Number.isInteger(maxAttempts) ||
    maxAttempts < 1 ||
    maxAttempts > MAX_ATTEMPTS_CEILING ||
    !Number.isFinite(baseDelayMs) ||
    baseDelayMs < 0 ||
    !Number.isFinite(maxDelayMs) ||
    maxDelayMs < 0
  ) {
    return 1;
  }
  return maxAttempts;
}

/** `min(maxDelayMs, base * 2^(attempt-1))` with +-20% jitter. */
export function retryDelayMs(
  policy: ProviderRetryPolicy,
  failedAttempt: number,
): number {
  const raw = Math.min(
    policy.maxDelayMs,
    policy.baseDelayMs * 2 ** (failedAttempt - 1),
  );
  const random = (policy.random ?? Math.random)();
  const unit = Number.isFinite(random) ? Math.min(Math.max(random, 0), 1) : 0.5;
  return Math.max(0, Math.round(raw * (0.8 + 0.4 * unit)));
}

export function defaultRetrySleep(
  ms: number,
  signal?: AbortSignal,
): Promise<void> {
  return new Promise<void>((resolve) => {
    if (signal?.aborted) {
      resolve();
      return;
    }
    const onAbort = (): void => {
      clearTimeout(timer);
      resolve();
    };
    const timer = setTimeout(() => {
      signal?.removeEventListener("abort", onAbort);
      resolve();
    }, ms);
    signal?.addEventListener("abort", onAbort, { once: true });
  });
}
