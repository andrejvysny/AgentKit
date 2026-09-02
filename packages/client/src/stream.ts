/**
 * `streamRun` — a run's event log as an async iterable that survives a dropped
 * connection.
 *
 * THE RESUME IS THE POINT. A run is minutes of provider streaming over a
 * connection that a laptop lid, a proxy idle timeout or a mobile handover will
 * break, and the durable log on the other side already knows how to hand back
 * exactly the tail that was missed: `Last-Event-ID` names the last event this
 * client actually saw, and the server replays from ONE PAST it (see
 * `packages/transport-http/src/sse.ts`). So a reconnect here is not a retry that
 * risks duplicates — it is the continuation the protocol was designed for, and
 * the caller never has to know it happened. Every event is yielded exactly once,
 * `seq`-contiguous across the seam — and that is ENFORCED here rather than
 * assumed of the server: a resume the server answered from the start of the log
 * (it did not recognise the id, which is its documented right) is de-duplicated
 * by `eventId` on the way out.
 *
 * WHAT ENDS THE ITERATION is the SERVER closing the stream, and nothing else.
 * The server closes on a terminal run event, and also when the task is terminal
 * with its log exhausted (a crashed attempt that never wrote one) — both mean
 * "nothing more is coming", and reconnecting after either would poll an ended
 * run forever. A clean end-of-body is therefore taken at face value; a
 * transport ERROR before a terminal event is what triggers the reconnect.
 *
 * WHAT DOES NOT END IT is the run's own outcome: a `run.failed` is a terminal
 * event, not an exception. The iterable yields it and stops. An exception from
 * this iterable always means the CALL failed — a 404 for a run that does not
 * exist, an abort, or a connection that broke more times than the retry budget
 * allows.
 *
 * TRAILING EVENTS. The host's correction harness can append `run.verification`
 * events AFTER the terminal event lands on the log (`TurnRunner`'s base pass
 * emits `run.completed` and the harness runs after it). A live stream has
 * already closed by then, so those events are invisible to it by construction —
 * {@link drainRun} is the one resumed pass that picks them up.
 */
import type { AiRunEvent } from "@agentkit/contracts";
import { AgentKitClientError } from "./errors.js";
import { parseSseStream, type SseFrame } from "./sse.js";
import type { RequestOptions, Transport } from "./transport.js";

/**
 * The run events after which the server sends nothing more.
 *
 * A MIRROR of `TERMINAL_RUN_EVENT_TYPES` in `@agentkit/transport-http`, restated
 * because this package sits beside that one rather than below it and must not
 * depend on a server adapter to talk to a server. The vocabulary is
 * `AiRunEventType`'s, which is the contract both read.
 */
export const TERMINAL_RUN_EVENT_TYPES: ReadonlySet<AiRunEvent["type"]> =
  new Set<AiRunEvent["type"]>(["run.completed", "run.failed", "run.cancelled"]);

/** Whether this event is a run's last word. */
export function isTerminalRunEvent(event: { type: string }): boolean {
  return TERMINAL_RUN_EVENT_TYPES.has(event.type as AiRunEvent["type"]);
}

export interface StreamRunOptions extends RequestOptions {
  /**
   * Resume from here instead of replaying the whole log — an `AiRunEvent.eventId`
   * the caller has already handled. An id the run does not know replays from the
   * beginning rather than failing, which is the server's rule, not this
   * client's: a full replay is the only answer that leaves a client consistent.
   */
  lastEventId?: string;
  /**
   * Reconnects allowed after a transport failure, per uninterrupted stretch.
   * The budget RESETS on every event received, so a long run that drops once an
   * hour is not spending down a lifetime allowance — what it bounds is a server
   * that accepts a connection and immediately drops it.
   */
  maxRetries?: number;
  /**
   * Reconnects allowed over the WHOLE iteration. Unlike {@link maxRetries} this
   * one never resets, and it is the budget that bounds the failure the
   * per-stretch one cannot see: a server that accepts a connection, delivers
   * one frame and drops it earns its stretch budget back every time, so it can
   * be reconnected to forever. Default {@link DEFAULT_STREAM_MAX_TOTAL_RECONNECTS}.
   */
  maxTotalReconnects?: number;
  /**
   * Delay before a reconnect, in milliseconds. The server's own `retry:` hint
   * overrides it once seen: the server knows its poll interval and its load,
   * and the whole reason SSE carries the field is so a client does not have to
   * guess.
   *
   * The HINT is clamped to
   * [{@link MIN_STREAM_RETRY_DELAY_MS}, {@link MAX_STREAM_RETRY_DELAY_MS}]; this
   * option is not. A `retry: 0` from a misconfigured or hostile server is a
   * reconnect loop with no pause in it, and a hint of an hour is a run the UI
   * silently stops following — neither is a policy a client should adopt just
   * because it arrived over the wire. A caller that really wants a 1 ms backoff
   * is asking for it on purpose and gets it.
   */
  retryDelayMs?: number;
}

export const DEFAULT_STREAM_MAX_RETRIES = 5;
export const DEFAULT_STREAM_RETRY_DELAY_MS = 500;
export const DEFAULT_STREAM_MAX_TOTAL_RECONNECTS = 50;
/** Floor for a server-supplied `retry:` hint. */
export const MIN_STREAM_RETRY_DELAY_MS = 250;
/** Ceiling for a server-supplied `retry:` hint. */
export const MAX_STREAM_RETRY_DELAY_MS = 30_000;

/**
 * How many recently yielded `eventId`s the de-dup remembers.
 *
 * Bounded because it guards a stream that can run for hours: the duplicate it
 * exists to catch is a REPLAY — a resume the server answered from the start of
 * the log instead of from `Last-Event-ID` — and a replay re-sends the tail this
 * client just saw, not something from the far past. A window is enough, and an
 * unbounded set on a long run is a leak.
 */
const DEDUPE_WINDOW = 4096;

interface StreamDeps {
  transport: Transport;
  runId: string;
}

/**
 * Open (and, as needed, re-open) the run's stream.
 *
 * Returned as an `AsyncIterable` rather than an `AsyncGenerator` because the
 * generator's `return`/`throw` half is not part of what this promises: a caller
 * `break`s out of a `for await`, and the generator's own `finally` cancels the
 * body.
 */
export function streamRun(
  deps: StreamDeps,
  options: StreamRunOptions = {},
): AsyncIterable<AiRunEvent> {
  return { [Symbol.asyncIterator]: () => iterate(deps, options) };
}

async function* iterate(
  deps: StreamDeps,
  options: StreamRunOptions,
): AsyncGenerator<AiRunEvent> {
  const maxRetries = options.maxRetries ?? DEFAULT_STREAM_MAX_RETRIES;
  const maxTotalReconnects =
    options.maxTotalReconnects ?? DEFAULT_STREAM_MAX_TOTAL_RECONNECTS;
  let retryDelayMs = options.retryDelayMs ?? DEFAULT_STREAM_RETRY_DELAY_MS;
  let lastEventId = options.lastEventId;
  let attempts = 0;
  let reconnects = 0;
  let sawTerminal = false;
  /** Recently yielded ids, so a replayed tail cannot be delivered twice. */
  const yielded = new Set<string>();

  for (;;) {
    try {
      for await (const frame of connect(deps, lastEventId, options)) {
        // The reconnect hint arrives before any event, on purpose: a client
        // that loses the connection on the very next byte already has the
        // server's policy — clamped, because that policy is the server's to
        // suggest and this client's to survive.
        if (frame.retry !== undefined) retryDelayMs = clampRetry(frame.retry);
        if (frame.data === undefined) continue;

        const event = parseEvent(frame.data);
        if (event === null) continue;

        // The frame's `id` is the server's own `eventId` for this event; the
        // event body is the fallback so a proxy that stripped the field cannot
        // silently disable resume.
        lastEventId = frame.id ?? event.eventId;
        // Progress earns back the budget: the failure this bounds is a
        // connection that never delivers, not a long run that drops twice.
        attempts = 0;
        // An id already handed to the caller is a REPLAY, not news: a server
        // that did not recognise the `Last-Event-ID` answers from the start of
        // the log, and a UI that appended the tail twice would show the answer
        // twice until the next reconcile. Its `retry:`/resume bookkeeping above
        // still counts — only the delivery is suppressed.
        if (yielded.has(event.eventId)) continue;
        remember(yielded, event.eventId);
        yield event;
        if (isTerminalRunEvent(event)) sawTerminal = true;
      }
      // The server closed. Terminal event or exhausted terminal task — either
      // way it has said everything it has to say.
      return;
    } catch (err) {
      if (options.signal?.aborted === true) throw err;
      // A problem response is an answer, not a broken pipe: a 404 for a run
      // that does not exist will be a 404 on every retry.
      if (err instanceof AgentKitClientError) throw err;
      // A break after the terminal event is the connection closing behind an
      // answer already delivered; reconnecting would replay nothing.
      if (sawTerminal) return;
      if (attempts >= maxRetries || reconnects >= maxTotalReconnects) throw err;
      attempts += 1;
      reconnects += 1;
      await sleep(retryDelayMs, options.signal);
    }
  }
}

/** A server's `retry:` hint, held to something a client can live with. */
function clampRetry(hint: number): number {
  if (hint < MIN_STREAM_RETRY_DELAY_MS) return MIN_STREAM_RETRY_DELAY_MS;
  if (hint > MAX_STREAM_RETRY_DELAY_MS) return MAX_STREAM_RETRY_DELAY_MS;
  return hint;
}

/** Add to the de-dup window, evicting the oldest id once it is full. */
function remember(yielded: Set<string>, eventId: string): void {
  yielded.add(eventId);
  if (yielded.size <= DEDUPE_WINDOW) return;
  const oldest = yielded.values().next();
  if (oldest.done !== true) yielded.delete(oldest.value);
}

/** One connection's frames. Throws on a non-2xx, which is not retryable. */
async function* connect(
  deps: StreamDeps,
  lastEventId: string | undefined,
  options: RequestOptions,
): AsyncGenerator<SseFrame> {
  const response = await deps.transport.request("streamRun", {
    path: { runId: deps.runId },
    accept: "text/event-stream",
    options: {
      ...options,
      headers: {
        ...(options.headers ?? {}),
        ...(lastEventId === undefined ? {} : { "last-event-id": lastEventId }),
      },
    },
  });

  if (response.body === null) {
    throw new TypeError(
      `The response to GET the stream of run ${deps.runId} carried no body.`,
    );
  }
  yield* parseSseStream(response.body);
}

/**
 * ONE resumed pass over whatever the log holds past `lastEventId`.
 *
 * This is how a caller collects the events the live stream could not see: the
 * host appends `run.verification` after the terminal event, and a stream that
 * closed at the terminal event closed before they were written. Because the task
 * is terminal by then, the server drains the remaining log and closes — so this
 * returns rather than following, and it does not reconnect: there is nothing to
 * wait for, and a caller that wants to keep watching wants {@link streamRun}.
 */
export async function drainRun(
  deps: StreamDeps,
  lastEventId?: string,
  options: RequestOptions = {},
): Promise<AiRunEvent[]> {
  const events: AiRunEvent[] = [];
  for await (const frame of connect(deps, lastEventId, options)) {
    if (frame.data === undefined) continue;
    const event = parseEvent(frame.data);
    if (event !== null) events.push(event);
  }
  return events;
}

/**
 * A frame body as an event, or `null`.
 *
 * A frame that does not parse is DROPPED rather than thrown on. The alternative
 * — failing the whole iteration — turns one corrupt frame from a middlebox into
 * a run the UI can never finish rendering, and the log itself is intact: the
 * next resume replays the run from its durable source.
 */
function parseEvent(data: string): AiRunEvent | null {
  try {
    const parsed: unknown = JSON.parse(data);
    if (parsed === null || typeof parsed !== "object") return null;
    if (typeof (parsed as { type?: unknown }).type !== "string") return null;
    return parsed as AiRunEvent;
  } catch {
    return null;
  }
}

/** Abortable sleep: a cancelled stream must not sit out its backoff. */
function sleep(ms: number, signal?: AbortSignal): Promise<void> {
  return new Promise<void>((resolve, reject) => {
    if (signal?.aborted === true) {
      reject(signal.reason);
      return;
    }
    const timer = setTimeout(() => {
      signal?.removeEventListener("abort", onAbort);
      resolve();
    }, ms);
    function onAbort(): void {
      clearTimeout(timer);
      reject(signal?.reason);
    }
    signal?.addEventListener("abort", onAbort, { once: true });
  });
}
