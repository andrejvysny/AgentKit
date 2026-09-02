import { AgentKitHostError } from "../errors.js";

/**
 * Deadlines for the four HOST hooks a turn calls out to.
 *
 * All four are host code the framework awaits inside a leased attempt, and none
 * of them was bounded before: a `ContextProvider` blocked on a socket, a
 * `ToolSetContributor` talking to a downed MCP server, a `VerificationHook`
 * waiting on a domain query — any of them parks the turn forever while the
 * runner keeps renewing the lease, and the chat becomes undeletable
 * (`chat_busy`) with no way back short of restarting the process.
 *
 * The numbers are per HOOK CLASS rather than per call, because what makes a
 * sensible ceiling is what the hook is for, not who wrote it: verification is
 * the slow one (it inspects the domain the run just wrote to), contributing a
 * tool set and resolving one attachment are both "fetch something small", and
 * context is read on the hot path of every pass.
 */
export interface HookTimeouts {
  /** {@link VerificationHook.verify}. Default 30 s. */
  verify?: number;
  /** {@link ContextProvider} `refresh` / `listBindings` / `systemPrompt`. Default 10 s. */
  context?: number;
  /** {@link AttachmentResolver.resolve}, per attachment. Default 10 s. */
  attachments?: number;
  /** {@link ToolSetContributor.contribute}, per contributor. Default 15 s. */
  contribute?: number;
}

/** Every field of {@link HookTimeouts}, resolved. */
export type ResolvedHookTimeouts = Required<HookTimeouts>;

export const DEFAULT_HOOK_TIMEOUTS_MS: ResolvedHookTimeouts = Object.freeze({
  verify: 30_000,
  context: 10_000,
  attachments: 10_000,
  contribute: 15_000,
});

export function resolveHookTimeouts(
  configured: HookTimeouts | undefined,
): ResolvedHookTimeouts {
  return { ...DEFAULT_HOOK_TIMEOUTS_MS, ...configured };
}

/** The `code` on the error {@link withHookDeadline} rejects with. */
export const HOOK_TIMEOUT_CODE = "hook_timeout";

/**
 * A host hook that did not answer inside its deadline.
 *
 * An {@link AgentKitHostError} with an ad hoc `code` rather than one of the
 * named subclasses: `HostErrorCode` is a closed, transport-mapped vocabulary,
 * and a hook that was slow is a degradation this layer handles itself — every
 * call site below turns it into a warning and a degraded turn, except the
 * single-shot verifier, which has always failed the turn when `verify()` threw
 * and keeps doing exactly that (now bounded instead of forever).
 */
export class HookTimeoutError extends AgentKitHostError {
  constructor(hook: string, timeoutMs: number) {
    super(
      HOOK_TIMEOUT_CODE,
      `Host hook "${hook}" did not answer within ${timeoutMs} ms.`,
      { hook, timeoutMs },
    );
  }
}

/** True for the rejection {@link withHookDeadline} raises on a timeout. */
export function isHookTimeout(err: unknown): err is HookTimeoutError {
  return err instanceof HookTimeoutError;
}

/**
 * Run a host hook under a deadline, rejecting with {@link HookTimeoutError} if
 * it has not settled in time.
 *
 * A RACE, not a cancellation. Nothing here can stop host code that is not
 * watching a signal, so a hook that answers late still answers — into a promise
 * nobody is holding, and its result is discarded. The turn has already gone on
 * without it, which is the whole point: the alternative is the turn not going
 * on at all. (The run's own `AbortSignal` is deliberately NOT aborted here: it
 * means "the user cancelled this turn", and firing it because a hook was slow
 * would cancel the run instead of degrading it.)
 *
 * A non-positive or non-finite `timeoutMs` means NO deadline, and the hook is
 * awaited exactly as it was before this existed — the escape hatch for a host
 * whose hook is legitimately unbounded. The timer is always cleared, including
 * on the fast path, so a settled hook never leaves a pending timer holding the
 * event loop open.
 */
export async function withHookDeadline<T>(input: {
  hook: string;
  timeoutMs: number;
  run: () => Promise<T>;
}): Promise<T> {
  const { hook, timeoutMs, run } = input;
  if (!Number.isFinite(timeoutMs) || timeoutMs <= 0) return run();
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    return await Promise.race([
      run(),
      new Promise<never>((_resolve, reject) => {
        timer = setTimeout(
          () => reject(new HookTimeoutError(hook, timeoutMs)),
          timeoutMs,
        );
      }),
    ]);
  } finally {
    if (timer !== undefined) clearTimeout(timer);
  }
}
